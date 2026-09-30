//! UPCR-2026-035 — host-registered tools of a host-owned app peer:
//! `peer/tools/register` authorization and replacement, exact tool
//! visibility per turn, routing to the host (`peer/tool/call` →
//! `peer/tool/result`), timeouts, and risk gating through the existing
//! approval path.
use super::*;

use crate::peers::host_tools::{apply_session_host_tools, resolve_session_host_tools};
use octos_core::ui_protocol::{ApprovalRespondParams, QuestionId, UserQuestionAnswer};

/// The chat id of this test's host sessions. Peer state (routes, the staged
/// peers on disk) is process-wide and tests run in parallel, so each test
/// (one thread per `#[tokio::test]`) gets its own id; every key in one test
/// shares it.
fn host_chat() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    thread_local! {
        static CHAT: String = format!("host-ht{}", NEXT.fetch_add(1, Ordering::Relaxed));
    }
    CHAT.with(Clone::clone)
}

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

/// [`fixture`], with `setup` applied to the server state first.
async fn fixture_with(setup: impl FnOnce(&mut AppState)) -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let profile = crate::profiles::UserProfile {
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
                        api_key_env: Some("HOST_TOOLS_TEST_KEY".to_string()),
                        api_type: None,
                    }),
                    ..Default::default()
                }),
                fallbacks: Vec::new(),
            }),
            env_vars: [("HOST_TOOLS_TEST_KEY".to_string(), "test-key".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        },
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
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

/// The registry of a turn on `key` driven by `connection`.
async fn turn_registry_on(
    fx: &Fx,
    key: &SessionKey,
    turn: &str,
    connection: u64,
) -> octos_agent::ToolRegistry {
    let runtime = crate::runtime::SessionRuntime::bootstrap(&fx.runtime, key.clone(), None)
        .await
        .expect("bound session");
    let mut registry = runtime.tools.snapshot_excluding(&[]);
    let resolved = resolve_session_host_tools(&peers_root(fx), key);
    apply_session_host_tools(
        &mut registry,
        &resolved,
        &peers_root(fx),
        key,
        turn,
        Some(connection),
    );
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

/// Next notification with `method` (skips any other frame).
async fn next_frame(rx: &mut mpsc::Receiver<WsMessage>, method: &str) -> Value {
    loop {
        let message = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
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

#[test]
fn should_advertise_and_dispatch_the_peer_tool_methods() {
    for method in [
        APPUI_METHOD_PEER_TOOLS_REGISTER,
        APPUI_METHOD_PEER_TOOL_RESULT,
        APPUI_METHOD_PEER_INPUT_REJECT,
    ] {
        assert!(APPUI_EXTRA_METHODS.contains(&method), "{method} advertised");
        assert!(
            raw_method_is_dispatched(method, false),
            "{method} dispatched"
        );
    }
}

#[tokio::test]
async fn should_refuse_a_registration_without_the_host_token() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let (ws, _rx) = ws_connection_for_test(8);
    let tools = json!({ "tools": [news_list()] });

    for bad in ["guess", ""] {
        let err = register(&fx, &ws, bad, tools.clone()).expect_err("wrong token");
        assert_eq!(err.data.unwrap()["kind"], "peer_host_token_mismatch");
    }
    let mut foreign = json!({
        "session_id": SessionKey::with_profile_topic("dev", "api", &host_chat(), "other"),
        "peer": "news", "host_token": token,
    });
    foreign["tools"] = tools["tools"].clone();
    let err = raw_peer_tools_register(
        &ws,
        &fx.state,
        &rpc(APPUI_METHOD_PEER_TOOLS_REGISTER, foreign),
        None,
    )
    .expect_err("foreign originator");
    assert_eq!(err.data.unwrap()["kind"], "peer_originator_mismatch");
    assert!(
        !peers_root(&fx).join("news/host_tools.json").exists(),
        "a refused registration writes nothing"
    );

    let invalid = register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [{"name": "list", "description": "x",
                           "input_schema": {"type": "object"}, "risk": "read"}] }),
    )
    .expect_err("un-namespaced name");
    assert_eq!(invalid.data.unwrap()["kind"], "peer_tools_invalid");

    let ok = register(&fx, &ws, &token, tools).expect("registered");
    assert_eq!(ok["version"], 1);
    assert_eq!(ok["tools"][0]["model_name"], "news_list");
    assert_eq!(ok["applies"], "next_turn");
}

#[tokio::test]
async fn should_offer_exactly_the_registered_tools_and_refuse_an_unlisted_one() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);

    // Before registration the peer keeps the ordinary roster.
    let before = turn_registry(&fx, &key, "turn-0").await;
    assert!(before.get("shell").is_some() && before.get("read_file").is_some());

    let (ws, _rx) = ws_connection_for_test(8);
    register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [news_list()], "generic_tools": ["read_file"] }),
    )
    .expect("registered");

    let registry = turn_registry(&fx, &key, "turn-1").await;
    assert_eq!(sorted_names(&registry), ["news_list", "read_file"]);
    let specs: Vec<String> = registry.specs().into_iter().map(|s| s.name).collect();
    assert!(!specs.iter().any(|name| name == "shell"), "{specs:?}");

    // Forcing an unlisted tool is refused by the registry itself.
    let forced = registry
        .execute_with_context(&call_ctx("c1"), "shell", &json!({"command": "id"}))
        .await;
    let Err(error) = forced else {
        panic!("an unlisted tool must be refused");
    };
    assert!(error.to_string().contains("unknown tool"), "{error}");

    // Request contexts of the peer get the same roster.
    let opened = raw_peer_context_open(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_CONTEXT_OPEN,
            json!({"session_id": fx.system, "peer": "news", "context_id": "mini-1",
                   "host_token": token}),
        ),
        None,
    )
    .expect("context open");
    let context_key: SessionKey = serde_json::from_value(opened["session_id"].clone()).unwrap();
    let registry = turn_registry(&fx, &context_key, "turn-2").await;
    assert_eq!(sorted_names(&registry), ["news_list", "read_file"]);

    // An unreadable set fails closed: no tools at all.
    std::fs::write(peers_root(&fx).join("news/host_tools.json"), "{torn").unwrap();
    let registry = turn_registry(&fx, &key, "turn-3").await;
    assert!(registry.tool_names().is_empty());
}

#[tokio::test]
async fn should_route_an_app_tool_call_to_the_host_and_back() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, rx) = ws_connection_for_test(32);
    register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [news_list()], "max_result_bytes": 64 }),
    )
    .unwrap();
    let host = spawn_fake_host(&fx, token.clone(), rx, |params| {
        if params["args"]["topic"] == "huge" {
            json!({ "ok": true, "data": {"items": ["x".repeat(200)]} })
        } else {
            json!({ "ok": true, "data": {"items": ["hn-1"], "topic": params["args"]["topic"]} })
        }
    });

    let registry = turn_registry(&fx, &key, "turn-1").await;
    let result = registry
        .execute_with_context(&call_ctx("c1"), "news_list", &json!({"topic": "rust"}))
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);
    let data: Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(data, json!({"items": ["hn-1"], "topic": "rust"}));

    let too_big = registry
        .execute_with_context(&call_ctx("c2"), "news_list", &json!({"topic": "huge"}))
        .await
        .unwrap();
    assert!(!too_big.success && too_big.output.contains("result_too_large"));

    drop(registry);
    drop(ws);
    crate::peers::host_tools::set_host_route(&peers_root(&fx), "news", 0, Arc::new(|_, _| false));
    let calls = host.await.unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["name"], "news.list");
    assert_eq!(calls[0]["session_id"], json!(key));
    assert_eq!(calls[0]["tool_call_id"], "c1");
    assert_eq!(calls[0]["risk"], "read");

    let rows = audit_rows(&fx);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["tool"], "news.list");
    assert_eq!(rows[0]["decision"], "allowed");
    assert_eq!(rows[0]["outcome"], "ok");
    assert_eq!(rows[1]["outcome"], "error:result_too_large");
    assert!(rows[0]["args_bytes"].as_u64().unwrap() > 0);
    assert!(rows[0]["result_bytes"].as_u64().unwrap() > 0);
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
    for _ in 0..200 {
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

#[tokio::test]
async fn should_not_run_a_declined_or_expired_destructive_call_nor_ask_twice() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, mut rx) = ws_connection_for_test(32);
    register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [mail_send()], "approval_ttl_secs": 1 }),
    )
    .unwrap();
    let registry = Arc::new(turn_registry(&fx, &key, "turn-1").await);
    let args = json!({"draft_id": "d-42"});
    let contracts = Arc::new(UiProtocolContractStores::default());
    let turn = TurnId::new();

    // Declined.
    let run = {
        let registry = registry.clone();
        let args = args.clone();
        let approver = app_approver(&fx, &key, &contracts, &turn);
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
    contracts
        .approvals
        .respond_with_context(ApprovalRespondParams::new(
            key.clone(),
            pending[0].approval_id.clone(),
            ApprovalDecision::Deny,
        ))
        .unwrap();
    let declined = run.await.unwrap();
    assert!(!declined.success && declined.output.contains("declined"));

    // The same occurrence (session/turn/tool_call_id) is never asked twice.
    let approver = app_approver(&fx, &key, &contracts, &turn);
    let again = octos_agent::tools::TOOL_APPROVAL_CTX
        .scope(
            approver.clone(),
            registry.execute_with_context(&call_ctx("c1"), "mail_send", &args),
        )
        .await
        .unwrap();
    assert!(!again.success && again.output.contains("not asked or sent twice"));
    assert!(contracts.approvals.pending_for_session(&key).is_empty());

    // Expired: nobody answers within the TTL; the parked approval is released.
    let expired = octos_agent::tools::TOOL_APPROVAL_CTX
        .scope(
            approver,
            registry.execute_with_context(&call_ctx("c2"), "mail_send", &args),
        )
        .await
        .unwrap();
    assert!(
        !expired.success && expired.output.contains("expired"),
        "{}",
        expired.output
    );
    assert!(
        contracts.approvals.pending_for_session(&key).is_empty(),
        "an expired approval does not stay pending"
    );

    // The host never saw a call.
    assert!(rx.try_recv().is_err());
    let decisions: Vec<Value> = audit_rows(&fx)
        .iter()
        .map(|r| r["decision"].clone())
        .collect();
    assert_eq!(
        decisions,
        [json!("denied"), json!("duplicate"), json!("expired")]
    );
}

#[tokio::test]
async fn should_replace_the_tool_set_atomically_and_refuse_a_stale_version() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let usual = sorted_names(&turn_registry(&fx, &key, "t0").await);
    let (ws, _rx) = ws_connection_for_test(8);
    register(&fx, &ws, &token, json!({ "tools": [news_list()] })).unwrap();
    let names = sorted_names(&turn_registry(&fx, &key, "t1").await);
    assert!(names.contains(&"news_list".to_owned()), "{names:?}");

    let mut read = news_list();
    read["name"] = json!("news.read");
    let v2 = register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [read], "if_version": 1 }),
    )
    .unwrap();
    assert_eq!(v2["version"], 2);
    assert_eq!(v2["previous_version"], 1);
    let names = sorted_names(&turn_registry(&fx, &key, "t2").await);
    assert!(
        names.contains(&"news_read".to_owned()) && !names.contains(&"news_list".to_owned()),
        "the old tool is gone, not merged: {names:?}"
    );

    let stale = register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [news_list()], "if_version": 1 }),
    )
    .expect_err("stale version");
    let data = stale.data.unwrap();
    assert_eq!(data["kind"], "peer_tools_version_conflict");
    assert_eq!(data["current_version"], 2);

    // An empty registration leaves the peer's usual kernel tools and no app
    // tool.
    register(&fx, &ws, &token, json!({ "tools": [] })).unwrap();
    assert_eq!(sorted_names(&turn_registry(&fx, &key, "t3").await), usual);
}

#[tokio::test]
async fn should_hand_a_confirm_app_call_to_the_owning_app_whoever_calls() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, rx) = ws_connection_for_test(32);
    register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [{
            "name": "news.share",
            "description": "Share a story with a contact.",
            "input_schema": {"type": "object", "required": ["story"]},
            "risk": "destructive",
            "confirm": "app",
            "background": true,
        }] }),
    )
    .unwrap();
    let host = spawn_fake_host(
        &fx,
        token.clone(),
        rx,
        |_| json!({ "ok": true, "data": {"shared": true} }),
    );
    let opened = raw_peer_context_open(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_CONTEXT_OPEN,
            json!({"session_id": fx.system, "peer": "news", "context_id": "ui-1",
                   "host_token": token}),
        ),
        None,
    )
    .unwrap();
    let context_key: SessionKey = serde_json::from_value(opened["session_id"].clone()).unwrap();
    let args = json!({"story": "hn-1"});

    // Person present (the app's own conversation): straight to the app's
    // confirmation sheet, no kernel approval.
    let contracts = Arc::new(UiProtocolContractStores::default());
    let registry = turn_registry(&fx, &context_key, "turn-1").await;
    let approver = app_approver(&fx, &context_key, &contracts, &TurnId::new());
    let present = octos_agent::tools::TOOL_APPROVAL_CTX
        .scope(
            approver,
            registry.execute_with_context(&call_ctx("c1"), "news_share", &args),
        )
        .await
        .unwrap();
    assert!(present.success, "{}", present.output);
    assert!(
        contracts
            .approvals
            .pending_for_session(&context_key)
            .is_empty()
    );

    // The peer's own session (no person in the app, e.g. the system agent's
    // input): still the owning app's own sheet, never a kernel approval.
    let registry = turn_registry(&fx, &key, "turn-2").await;
    let approver = app_approver(&fx, &key, &contracts, &TurnId::new());
    let absent = octos_agent::tools::TOOL_APPROVAL_CTX
        .scope(
            approver,
            registry.execute_with_context(&call_ctx("c2"), "news_share", &args),
        )
        .await
        .unwrap();
    assert!(absent.success, "{}", absent.output);
    assert!(contracts.approvals.pending_for_session(&key).is_empty());

    drop(ws);
    crate::peers::host_tools::set_host_route(&peers_root(&fx), "news", 0, Arc::new(|_, _| false));
    let calls = host.await.unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["context_id"], "ui-1");
    assert_eq!(calls[0]["confirm_required"], true);
    assert_eq!(calls[1]["context_id"], Value::Null);
    assert_eq!(calls[1]["confirm_required"], true);
    let decisions: Vec<Value> = audit_rows(&fx)
        .iter()
        .map(|r| r["decision"].clone())
        .collect();
    assert_eq!(decisions, [json!("app_confirms"), json!("app_confirms")]);
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

#[tokio::test]
async fn should_report_an_unanswered_act_call_as_unknown_and_never_resend_it() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, mut rx) = ws_connection_for_test(32);
    register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [news_topics_set()], "call_timeout_ms": 100 }),
    )
    .unwrap();
    let registry = turn_registry(&fx, &key, "turn-1").await;
    let args = json!({"topics": ["rust"]});
    let result = registry
        .execute_with_context(&call_ctx("c1"), "news_topics_set", &args)
        .await
        .unwrap();
    assert!(!result.success, "{}", result.output);
    assert!(
        result.output.contains("outcome_unknown") && result.output.contains("Do not retry"),
        "{}",
        result.output
    );
    let call = next_frame(&mut rx, "peer/tool/call").await;
    assert!(call["args_digest"].as_str().unwrap().starts_with("sha256:"));
    assert_eq!(
        next_frame(&mut rx, "peer/tool/cancel").await["reason"],
        "timeout"
    );

    // A re-dispatch of the same call never reaches the host again.
    let again = registry
        .execute_with_context(&call_ctx("c1"), "news_topics_set", &args)
        .await
        .unwrap();
    assert!(!again.success && again.output.contains("not asked or sent twice"));
    // Nor does a retry under a new tool-call id with the same arguments.
    let retry = registry
        .execute_with_context(&call_ctx("c2"), "news_topics_set", &args)
        .await
        .unwrap();
    assert!(
        !retry.success && retry.output.contains("not sent again"),
        "{}",
        retry.output
    );
    assert!(rx.try_recv().is_err(), "no second peer/tool/call");

    // A later turn of the same session does not resend it either.
    let later = turn_registry(&fx, &key, "turn-2").await;
    let again = later
        .execute_with_context(&call_ctx("c1"), "news_topics_set", &args)
        .await
        .unwrap();
    assert!(
        !again.success && again.output.contains("not sent again"),
        "{}",
        again.output
    );
    assert!(rx.try_recv().is_err(), "no second peer/tool/call");

    // Nor does another request context of the same peer (#2572): the mark is
    // keyed on the peer, not on the calling session.
    let _ = raw_peer_context_open(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_CONTEXT_OPEN,
            json!({"session_id": fx.system, "peer": "news", "context_id": "ui-1",
                   "host_token": token}),
        ),
        None,
    )
    .unwrap();
    let ctx_key = SessionKey(format!("{}#peerctx-news.ui-1", fx.system.base_key()));
    let other = turn_registry(&fx, &ctx_key, "turn-3").await;
    let again = other
        .execute_with_context(&call_ctx("c3"), "news_topics_set", &args)
        .await
        .unwrap();
    assert!(
        !again.success && again.output.contains("not sent again"),
        "{}",
        again.output
    );
    assert!(rx.try_recv().is_err(), "no peer/tool/call from the context");

    let rows = audit_rows(&fx);
    assert_eq!(rows[0]["outcome"], "unknown");
    assert_eq!(rows[1]["decision"], "duplicate");
}

#[tokio::test]
async fn should_treat_a_call_interrupted_while_the_host_worked_as_unknown() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, mut rx) = ws_connection_for_test(32);
    register(&fx, &ws, &token, json!({ "tools": [news_topics_set()] })).unwrap();
    let registry = Arc::new(turn_registry(&fx, &key, "turn-1").await);
    let args = json!({"topics": ["rust"]});
    let task = {
        let registry = registry.clone();
        let args = args.clone();
        tokio::spawn(async move {
            registry
                .execute_with_context(&call_ctx("c1"), "news_topics_set", &args)
                .await
        })
    };
    next_frame(&mut rx, "peer/tool/call").await;
    // The turn is interrupted while the host is working on the call.
    task.abort();
    let _ = task.await;
    assert_eq!(
        next_frame(&mut rx, "peer/tool/cancel").await["reason"],
        "cancelled"
    );

    let later = turn_registry(&fx, &key, "turn-2").await;
    let again = later
        .execute_with_context(&call_ctx("c9"), "news_topics_set", &args)
        .await
        .unwrap();
    assert!(
        !again.success && again.output.contains("not sent again"),
        "{}",
        again.output
    );
    assert!(rx.try_recv().is_err(), "not sent to the host again");
}

#[tokio::test]
async fn should_give_no_tools_to_a_kernel_internal_continuation() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, _rx) = ws_connection_for_test(8);
    register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [news_list()], "generic_tools": ["read_file"] }),
    )
    .unwrap();
    let runtime = crate::runtime::SessionRuntime::bootstrap(&fx.runtime, key.clone(), None)
        .await
        .unwrap();
    let mut registry = runtime.tools.snapshot_excluding(&[]);
    let resolved = resolve_session_host_tools(&peers_root(&fx), &key);
    // `run_standalone_turn` passes no connection for an internal continuation.
    apply_session_host_tools(&mut registry, &resolved, &peers_root(&fx), &key, "t", None);
    assert!(registry.tool_names().is_empty());
}

#[tokio::test]
async fn should_clamp_host_filesystem_access_for_a_bound_app_session() {
    let fx = fixture().await;
    prepare_news(&fx).await;
    let key = peer_key(&fx);
    let runtime = crate::runtime::SessionRuntime::bootstrap_with_permissions(
        &fx.runtime,
        key,
        None,
        octos_agent::EffectivePermissions::danger_full_access(),
    )
    .await
    .expect("bound session");
    assert!(
        !runtime.permissions.filesystem_scope.is_host(),
        "a bound app session never gets host filesystem access"
    );
    assert!(
        runtime.agent.session_scope().is_some(),
        "file tools are fenced"
    );

    // An ordinary session keeps the operator's grant.
    let plain = crate::runtime::SessionRuntime::bootstrap_with_permissions(
        &fx.runtime,
        SessionKey::with_profile_topic("dev", "api", &host_chat(), "plain"),
        None,
        octos_agent::EffectivePermissions::danger_full_access(),
    )
    .await
    .unwrap();
    assert!(plain.permissions.filesystem_scope.is_host());
}

fn rpc_error_kind(message: WsMessage) -> Value {
    frame_json(message)["error"]["data"]["kind"].clone()
}

#[tokio::test]
async fn should_keep_a_host_tool_approval_and_turn_controls_on_the_host_connection() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (host_ws, host_rx) = ws_connection_for_test(64);
    register(&fx, &host_ws, &token, json!({ "tools": [mail_send()] })).unwrap();
    let host = spawn_fake_host(
        &fx,
        token.clone(),
        host_rx,
        |_| json!({ "ok": true, "data": {} }),
    );
    let registry = Arc::new(turn_registry(&fx, &key, "turn-1").await);
    let ledger = Arc::new(UiProtocolLedger::new(64));
    let contracts = Arc::new(UiProtocolContractStores::default());
    let turn = TurnId::new();
    // The host's own turn: its approval bridge writes to the shared ledger.
    let (bridge_ws, _bridge_rx) = ws_connection_for_test(64);
    let approver: Arc<dyn octos_agent::ToolApprovalRequester> =
        Arc::new(UiProtocolApprovalRequester {
            ws: bridge_ws,
            ledger: ledger.clone(),
            contracts: contracts.clone(),
            state: fx.state.clone(),
            peers_root: peers_root(&fx),
            session_id: key.clone(),
            turn_id: turn.clone(),
            features: ConnectionUiFeatures::default(),
        });
    let run = {
        let registry = registry.clone();
        tokio::spawn(
            octos_agent::tools::TOOL_APPROVAL_CTX.scope(approver, async move {
                registry
                    .execute_with_context(&call_ctx("c1"), "mail_send", &json!({"draft_id": "d"}))
                    .await
                    .unwrap()
            }),
        )
    };
    let pending = wait_for_pending(&contracts, &key).await;
    let approval_id = pending[0].approval_id.clone();

    // Another connection of the profile, on the same session.
    let (spoof_ws, mut spoof_rx) = ws_connection_for_test(64);
    let mut requested = None;
    for _ in 0..200 {
        requested = ledger
            .replay_after(
                &key,
                Some(&UiCursor {
                    stream: key.0.clone(),
                    seq: 0,
                }),
            )
            .unwrap()
            .into_iter()
            .find(|e| {
                matches!(
                    &e.event,
                    UiProtocolLedgerEvent::Notification(UiNotification::ApprovalRequested(_))
                )
            });
        if requested.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let requested = requested.expect("the approval is in the shared ledger");
    // Neither live forwarding nor replay shows it to that connection...
    assert!(!ledger_event_visible_to_connection(
        &requested.event,
        spoof_ws.connection_id
    ));
    assert!(ledger_event_visible_to_connection(
        &requested.event,
        host_ws.connection_id
    ));
    forward_live_ledger_event(
        &spoof_ws,
        &ledger,
        requested.clone(),
        0,
        spoof_ws.connection_id,
        ConnectionUiFeatures::default(),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(spoof_rx.try_recv().is_err(), "not forwarded live");

    // ...and it cannot answer it.
    let respond = |approval_id: ApprovalId| {
        ApprovalRespondParams::new(key.clone(), approval_id, ApprovalDecision::Approve)
    };
    handle_approval_respond(
        &spoof_ws,
        &fx.state,
        &ledger,
        &contracts,
        None,
        None,
        "r1".into(),
        respond(approval_id.clone()),
    )
    .await;
    assert_eq!(
        rpc_error_kind(spoof_rx.recv().await.unwrap()),
        "peer_host_connection_only"
    );
    assert_eq!(contracts.approvals.pending_for_session(&key).len(), 1);

    // Nor steer or interrupt the host's turn.
    let active_turns: SharedActiveTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    handle_turn_interrupt(
        &spoof_ws,
        &fx.state,
        &ledger,
        &active_turns,
        &contracts,
        "i1".into(),
        TurnInterruptParams {
            session_id: key.clone(),
            turn_id: turn.clone(),
        },
    )
    .await;
    assert_eq!(
        rpc_error_kind(spoof_rx.recv().await.unwrap()),
        "peer_host_connection_only"
    );
    assert!(refuse_foreign_host_turn_control(&fx.state, &key, &spoof_ws, "turn/steer").is_some());
    assert!(refuse_foreign_host_turn_control(&fx.state, &key, &host_ws, "turn/steer").is_none());

    // The host connection answers it.
    handle_approval_respond(
        &host_ws,
        &fx.state,
        &ledger,
        &contracts,
        None,
        None,
        "r2".into(),
        respond(approval_id),
    )
    .await;
    assert!(run.await.unwrap().success);
    drop(host_ws);
    crate::peers::host_tools::set_host_route(&peers_root(&fx), "news", 0, Arc::new(|_, _| false));
    assert_eq!(host.await.unwrap().len(), 1);
}

#[tokio::test]
async fn should_wait_for_the_apps_confirmation_sheet_instead_of_timing_out() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let (ws, mut rx) = ws_connection_for_test(32);
    register(
        &fx,
        &ws,
        &token,
        json!({
            "tools": [{
                "name": "news.share",
                "description": "Share a story with a contact.",
                "input_schema": {"type": "object", "required": ["story"]},
                "risk": "destructive",
                "confirm": "app",
            }, mail_send()],
            "call_timeout_ms": 100,
            "approval_ttl_secs": 30,
        }),
    )
    .unwrap();
    let opened = raw_peer_context_open(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_CONTEXT_OPEN,
            json!({"session_id": fx.system, "peer": "news", "context_id": "ui-1",
                   "host_token": token}),
        ),
        None,
    )
    .unwrap();
    let context_key: SessionKey = serde_json::from_value(opened["session_id"].clone()).unwrap();
    let registry = Arc::new(turn_registry(&fx, &context_key, "turn-1").await);
    let contracts = Arc::new(UiProtocolContractStores::default());

    // confirm=app, person present: the sheet takes longer than the call
    // timeout; the kernel keeps waiting and sends no cancel.
    let run = {
        let registry = registry.clone();
        let approver = app_approver(&fx, &context_key, &contracts, &TurnId::new());
        tokio::spawn(
            octos_agent::tools::TOOL_APPROVAL_CTX.scope(approver, async move {
                registry
                    .execute_with_context(&call_ctx("c1"), "news_share", &json!({"story": "hn-1"}))
                    .await
                    .unwrap()
            }),
        )
    };
    let call = next_frame(&mut rx, "peer/tool/call").await;
    assert_eq!(call["confirm_required"], true);
    assert!(call["timeout_ms"].as_u64().unwrap() >= 30_000);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    answer(
        &fx,
        &token,
        call["call_id"].as_str().unwrap(),
        json!({"ok": true, "data": {"shared": 1}}),
    )
    .expect("the answer after the sheet is accepted");
    assert!(run.await.unwrap().success);

    // An approved confirm=host call: the host acknowledges it is still asking
    // the person, which extends the wait past the call timeout.
    let run = {
        let registry = registry.clone();
        let approver = app_approver(&fx, &context_key, &contracts, &TurnId::new());
        tokio::spawn(
            octos_agent::tools::TOOL_APPROVAL_CTX.scope(approver, async move {
                registry
                    .execute_with_context(&call_ctx("c2"), "mail_send", &json!({"draft_id": "d-1"}))
                    .await
                    .unwrap()
            }),
        )
    };
    let pending = wait_for_pending(&contracts, &context_key).await;
    contracts
        .approvals
        .respond_with_context(ApprovalRespondParams::new(
            context_key.clone(),
            pending[0].approval_id.clone(),
            ApprovalDecision::Approve,
        ))
        .unwrap();
    let call = next_frame(&mut rx, "peer/tool/call").await;
    let call_id = call["call_id"].as_str().unwrap().to_owned();
    let ack = answer(
        &fx,
        &token,
        &call_id,
        json!({"status": "awaiting_confirmation"}),
    )
    .unwrap();
    assert_eq!(ack["awaiting_confirmation"], true);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    answer(
        &fx,
        &token,
        &call_id,
        json!({"ok": true, "data": {"sent": true}}),
    )
    .unwrap();
    assert!(run.await.unwrap().success);
    while let Ok(frame) = rx.try_recv() {
        assert_ne!(frame_json(frame)["method"], "peer/tool/cancel");
    }
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

#[tokio::test]
async fn should_give_no_tools_to_a_session_on_a_foreign_base_key() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let (ws, _rx) = ws_connection_for_test(8);
    register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [news_list(), mail_send()], "generic_tools": ["read_file"] }),
    )
    .unwrap();
    let _ = raw_peer_context_open(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_CONTEXT_OPEN,
            json!({"session_id": fx.system, "peer": "news", "context_id": "ui-1",
                   "host_token": token}),
        ),
        None,
    )
    .unwrap();
    let foreign_base = SessionKey::with_profile_topic("dev", "api", "intruder", "x");
    for topic in ["peerctx-news.ui-1", "peer-news"] {
        let key = SessionKey(format!("{}#{topic}", foreign_base.base_key()));
        let resolved = resolve_session_host_tools(&peers_root(&fx), &key);
        assert!(
            matches!(
                resolved,
                crate::peers::host_tools::SessionHostTools::FailClosed { .. }
            ),
            "{topic}: {resolved:?}"
        );
        let mut registry = fx.runtime.tool_specs.snapshot_excluding(&[]);
        assert!(!registry.tool_names().is_empty());
        apply_session_host_tools(
            &mut registry,
            &resolved,
            &peers_root(&fx),
            &key,
            "t",
            Some(ws.connection_id.0),
        );
        assert!(registry.tool_names().is_empty(), "{topic}");
    }
    // The owner's own sessions still get the set.
    let own = SessionKey(format!("{}#peerctx-news.ui-1", fx.system.base_key()));
    assert!(matches!(
        resolve_session_host_tools(&peers_root(&fx), &own),
        crate::peers::host_tools::SessionHostTools::Enforced { .. }
    ));
}

#[tokio::test]
async fn should_offer_exactly_the_generic_tools_the_host_sets() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let usual = sorted_names(&turn_registry(&fx, &key, "t0").await);
    let (ws, _rx) = ws_connection_for_test(8);
    // No hard-coded exclusions: research, command execution, whatever the
    // app declared and the host granted.
    register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [news_list()],
                "generic_tools": ["web_search", "deep_search", "shell", "read_file"] }),
    )
    .expect("any kernel tool name");
    let names = sorted_names(&turn_registry(&fx, &key, "t1").await);
    let mut expected: Vec<String> = ["web_search", "deep_search", "shell", "read_file"]
        .into_iter()
        .filter(|t| usual.contains(&t.to_string()))
        .map(str::to_owned)
        .collect();
    expected.push("news_list".into());
    expected.sort();
    assert_eq!(names, expected, "exactly the host's set plus the app tool");
    assert!(names.contains(&"shell".to_owned()));
    // An explicit empty list: no kernel tools at all, only the app's.
    register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [news_list()], "generic_tools": [] }),
    )
    .unwrap();
    assert_eq!(
        sorted_names(&turn_registry(&fx, &key, "t2").await),
        ["news_list"]
    );
    // The list never adds a tool the peer's session does not have.
    register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [], "generic_tools": ["peer_send_input", "no_such_tool"] }),
    )
    .unwrap();
    assert!(turn_registry(&fx, &key, "t3").await.tool_names().is_empty());
    // Names must look like kernel tool names (an app tool is dotted).
    for bad in ["news.list", "rm -rf", ""] {
        let err = register(&fx, &ws, &token, json!({ "generic_tools": [bad] })).expect_err(bad);
        assert_eq!(err.data.unwrap()["kind"], "peer_tools_invalid", "{bad}");
    }
    let err = register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [{"name": "news.list", "description": "x",
                           "input_schema": {"type": "object",
                                            "properties": {"n": {"type": "whole"}}},
                           "risk": "read"}] }),
    )
    .expect_err("bad schema type");
    assert!(
        err.message.contains("not a JSON Schema type"),
        "{}",
        err.message
    );

    // Boolean subschemas and additionalProperties are honoured.
    register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [{"name": "news.list", "description": "x",
                           "input_schema": {"type": "object",
                                            "properties": {"any": true},
                                            "additionalProperties": false},
                           "risk": "read"}],
                "generic_tools": ["read_file", "deep_search", "memory_search"] }),
    )
    .expect("boolean subschemas are valid");
    let err = register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [{"name": "news.list", "description": "x",
                           "input_schema": {"type": "object",
                                            "additionalProperties": {"type": 7}},
                           "risk": "read"}] }),
    )
    .expect_err("bad additionalProperties");
    assert!(
        err.message.contains("additionalProperties"),
        "{}",
        err.message
    );

    // A set stored before `generic_tools` became optional (a plain list)
    // still reads as that exact list.
    let path = peers_root(&fx).join("news/host_tools.json");
    let mut stored: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    stored["generic_tools"] = json!(["read_file"]);
    std::fs::write(&path, stored.to_string()).unwrap();
    assert_eq!(
        sorted_names(&turn_registry(&fx, &key, "t4").await),
        ["news_list", "read_file"]
    );
}

#[tokio::test]
async fn should_give_no_tools_to_another_connection_on_the_hosts_base_key() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let (host_ws, _host_rx) = ws_connection_for_test(32);
    register(
        &fx,
        &host_ws,
        &token,
        json!({ "tools": [news_list(), mail_send()] }),
    )
    .unwrap();
    let opened = raw_peer_context_open(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_CONTEXT_OPEN,
            json!({"session_id": fx.system, "peer": "news", "context_id": "ui-1",
                   "host_token": token}),
        ),
        None,
    )
    .unwrap();
    let context_key: SessionKey = serde_json::from_value(opened["session_id"].clone()).unwrap();

    // Another client of the profile opens the same topic on the host's base
    // key and drives a turn on its own connection.
    let (spoof_ws, mut spoof_rx) = ws_connection_for_test(32);
    for key in [context_key.clone(), peer_key(&fx)] {
        let registry = turn_registry_on(&fx, &key, "turn-x", spoof_ws.connection_id.0).await;
        assert!(registry.tool_names().is_empty(), "{key:?}");

        // It cannot call the destructive tool, so no approval is ever raised
        // for it to receive or answer.
        let contracts = Arc::new(UiProtocolContractStores::default());
        let approver: Arc<dyn octos_agent::ToolApprovalRequester> =
            Arc::new(UiProtocolApprovalRequester {
                ws: spoof_ws.clone(),
                ledger: Arc::new(UiProtocolLedger::new(64)),
                contracts: contracts.clone(),
                state: fx.state.clone(),
                peers_root: peers_root(&fx),
                session_id: key.clone(),
                turn_id: TurnId::new(),
                features: ConnectionUiFeatures::default(),
            });
        let forced = octos_agent::tools::TOOL_APPROVAL_CTX
            .scope(
                approver,
                registry.execute_with_context(
                    &call_ctx("c1"),
                    "mail_send",
                    &json!({"draft_id": "d"}),
                ),
            )
            .await;
        assert!(forced.is_err(), "unknown tool");
        assert!(contracts.approvals.pending_for_session(&key).is_empty());
    }
    assert!(
        spoof_rx.try_recv().is_err(),
        "nothing reached the spoofing connection"
    );

    // The host's own connection still gets the set; once it closes, nobody does.
    let registry = turn_registry_on(&fx, &context_key, "turn-h", host_ws.connection_id.0).await;
    assert!(registry.get("mail_send").is_some() && registry.get("news_list").is_some());
    crate::peers::host_tools::drop_routes_for_connection(host_ws.connection_id.0);
    let registry = turn_registry_on(&fx, &context_key, "turn-h2", host_ws.connection_id.0).await;
    assert!(registry.tool_names().is_empty());
}

#[tokio::test]
async fn should_refuse_an_awaiting_confirmation_ack_for_a_call_that_is_not_gated() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, mut rx) = ws_connection_for_test(32);
    register(&fx, &ws, &token, json!({ "tools": [news_topics_set()] })).unwrap();
    let registry = Arc::new(turn_registry(&fx, &key, "turn-1").await);
    let run = {
        let registry = registry.clone();
        tokio::spawn(async move {
            registry
                .execute_with_context(&call_ctx("c1"), "news_topics_set", &json!({"topics": []}))
                .await
                .unwrap()
        })
    };
    let call = next_frame(&mut rx, "peer/tool/call").await;
    let call_id = call["call_id"].as_str().unwrap().to_owned();
    let err = answer(
        &fx,
        &token,
        &call_id,
        json!({"status": "awaiting_confirmation"}),
    )
    .expect_err("an act call is not acknowledged");
    assert_eq!(err.data.unwrap()["kind"], "peer_tool_ack_not_gated");
    answer(&fx, &token, &call_id, json!({"ok": true, "data": {}})).unwrap();
    assert!(run.await.unwrap().success);
}

/// Round-4 review regression: a runtime cached for `<host base>#peer-news`
/// BEFORE `peer/prepare` bound the topic must never be reused afterwards —
/// it carries the profile's memory, the profile workspace and unclamped
/// host filesystem access.
#[tokio::test]
async fn should_rebuild_a_session_runtime_cached_before_the_peer_was_bound() {
    let fx = fixture().await;
    let key = peer_key(&fx);
    let cache = &fx.state.session_cache;
    let epoch = cache.session_generation(&key);
    let stale = cache
        .get_or_init_with_permissions(
            &fx.runtime,
            key.clone(),
            None,
            octos_agent::EffectivePermissions::danger_full_access(),
            epoch,
        )
        .await
        .expect("an ordinary session before the peer exists");
    assert!(stale.permissions.filesystem_scope.is_host());
    assert!(stale.memory.namespace.is_none());

    let token = prepare_news(&fx).await;
    let (ws, _rx) = ws_connection_for_test(8);
    register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [news_list()], "generic_tools": ["read_file", "memory_search"] }),
    )
    .unwrap();

    let epoch = cache.session_generation(&key);
    let fresh = cache
        .get_or_init_with_permissions(
            &fx.runtime,
            key.clone(),
            None,
            octos_agent::EffectivePermissions::danger_full_access(),
            epoch,
        )
        .await
        .expect("the bound session");
    assert!(
        !Arc::ptr_eq(&stale, &fresh),
        "the stale runtime is not reused"
    );
    assert!(!fresh.permissions.filesystem_scope.is_host(), "clamped");
    assert!(fresh.agent.session_scope().is_some(), "scoped");
    assert_eq!(fresh.memory.namespace.as_deref(), Some("app/news/acct-1"));
    assert_eq!(
        dunce::canonicalize(&fresh.workspace_root).unwrap(),
        dunce::canonicalize(fx.apps.join("news")).unwrap()
    );

    // The host's turn roster is built on the rebuilt runtime.
    let mut registry = fresh.tools.snapshot_excluding(&[]);
    let resolved = resolve_session_host_tools(&peers_root(&fx), &key);
    apply_session_host_tools(
        &mut registry,
        &resolved,
        &peers_root(&fx),
        &key,
        "t",
        Some(ws.connection_id.0),
    );
    assert_eq!(
        sorted_names(&registry),
        ["memory_search", "news_list", "read_file"]
    );

    // A context topic cached before `peer/context/open` is dropped by the
    // bind-time invalidation as well.
    let ctx_key = SessionKey(format!("{}#peerctx-news.ui-9", fx.system.base_key()));
    let epoch = cache.session_generation(&ctx_key);
    // Never opened: it cannot even bootstrap.
    assert!(
        cache
            .get_or_init_with_permissions(
                &fx.runtime,
                ctx_key.clone(),
                None,
                octos_agent::EffectivePermissions::workspace_write(),
                epoch,
            )
            .await
            .is_err()
    );
    invalidate_bound_topics(&fx.state, std::iter::once(&json!({"topic": "peer-news"}))).await;
    let epoch = cache.session_generation(&key);
    let again = cache
        .get_or_init_with_permissions(
            &fx.runtime,
            key.clone(),
            None,
            octos_agent::EffectivePermissions::workspace_write(),
            epoch,
        )
        .await
        .unwrap();
    assert!(
        !Arc::ptr_eq(&fresh, &again),
        "bind-time invalidation drops the entry"
    );
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
        let usage = octos_llm::TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        };
        if call == 0 {
            Ok(octos_llm::ChatResponse {
                content: None,
                reasoning_content: None,
                tool_calls: vec![octos_core::ToolCall {
                    id: "call_1".into(),
                    name: self.tool.clone(),
                    arguments: self.args.clone(),
                    metadata: None,
                }],
                stop_reason: octos_llm::StopReason::ToolUse,
                usage,
                provider_index: None,
            })
        } else {
            Ok(octos_llm::ChatResponse {
                content: Some("done".into()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                stop_reason: octos_llm::StopReason::EndTurn,
                usage,
                provider_index: None,
            })
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
            &crate::profiles::UserProfile {
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
                                api_key_env: Some("HOST_TOOLS_E2E_KEY".to_string()),
                                api_type: None,
                            }),
                            ..Default::default()
                        }),
                        fallbacks: Vec::new(),
                    }),
                    env_vars: [("HOST_TOOLS_E2E_KEY".to_string(), "k".to_string())]
                        .into_iter()
                        .collect(),
                    ..Default::default()
                },
                created_at: Utc::now(),
                updated_at: Utc::now(),
            },
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

/// Drive one `turn/start` on `session` from the host connection. The fake
/// host answers `peer/tool/call` with `{"items": ["hn-1"]}` and approves any
/// `approval/requested`. Returns the tool calls the host saw and the
/// approvals it was asked.
async fn e2e_turn(
    e: &mut E2e,
    session: &SessionKey,
    llm: &ScriptedHostToolLlm,
) -> (Vec<Value>, Vec<Value>) {
    e2e_turn_with(e, session, llm, "what's new?", TurnId::new()).await
}

/// [`e2e_turn`] with the turn's input text and id.
async fn e2e_turn_with(
    e: &mut E2e,
    session: &SessionKey,
    llm: &ScriptedHostToolLlm,
    text: &str,
    turn_id: TurnId,
) -> (Vec<Value>, Vec<Value>) {
    let ledger = Arc::new(UiProtocolLedger::new(256));
    let contracts = Arc::new(UiProtocolContractStores::default());
    let active_turns: SharedActiveTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let connection_turns: SharedConnectionTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let mut rx = e.rx.take().unwrap();
    let state = e.state.clone();
    let system = e.system.clone();
    let token = e.token.clone();
    let connection = e.ws.connection_id.0;
    let host_contracts = contracts.clone();
    let host_session = session.clone();
    let host = tokio::spawn(async move {
        let mut calls = Vec::new();
        let mut approvals = Vec::new();
        while let Some(message) = rx.recv().await {
            let WsMessage::Text(text) = message else {
                continue;
            };
            let frame: Value = serde_json::from_str(text.as_str()).unwrap();
            match frame["method"].as_str() {
                Some("peer/tool/call") => {
                    let params = frame["params"].clone();
                    raw_peer_tool_result(
                        connection,
                        &state,
                        &rpc(
                            APPUI_METHOD_PEER_TOOL_RESULT,
                            json!({
                                "session_id": system, "peer": "news", "host_token": token,
                                "call_id": params["call_id"], "ok": true,
                                "data": {"items": ["hn-1"]},
                            }),
                        ),
                        None,
                    )
                    .expect("result accepted");
                    calls.push(params);
                }
                Some("approval/requested") => {
                    let params = frame["params"].clone();
                    let approval_id: ApprovalId =
                        serde_json::from_value(params["approval_id"].clone()).unwrap();
                    host_contracts
                        .approvals
                        .respond_with_context(ApprovalRespondParams::new(
                            host_session.clone(),
                            approval_id,
                            ApprovalDecision::Approve,
                        ))
                        .expect("the person approves in the app");
                    approvals.push(params);
                }
                _ => {}
            }
            if frame["method"] == "__stop__" {
                break;
            }
        }
        (calls, approvals)
    });
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
            turn_id,
            input: vec![InputItem::Text { text: text.into() }],
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
    // Generous: under a loaded test run the turn can take seconds.
    for _ in 0..3000 {
        if llm.calls.load(std::sync::atomic::Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        llm.calls.load(std::sync::atomic::Ordering::SeqCst) >= 2,
        "the turn ran to its second model call"
    );
    // Let the turn finish, then stop the fake host.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let _ = send_raw_notification_ephemeral(&e.ws, "__stop__", json!({}));
    tokio::time::timeout(std::time::Duration::from_secs(5), host)
        .await
        .expect("fake host stops")
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_route_a_real_turns_app_tool_call_to_the_host_end_to_end() {
    let llm = ScriptedHostToolLlm::new("news_list", json!({"topic": "rust"}));
    let mut e = e2e_fixture(
        llm.clone(),
        json!({ "tools": [news_list()], "generic_tools": ["read_file"] }),
    )
    .await;
    let session = SessionKey(format!("{}#peer-news", e.system.base_key()));
    let (calls, approvals) = e2e_turn(&mut e, &session, &llm).await;

    // The model was offered exactly the registered set.
    assert_eq!(llm.rosters.lock().unwrap()[0], ["news_list", "read_file"]);
    // Its tool call reached the host, and the host's data reached the model.
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["name"], "news.list");
    assert_eq!(calls[0]["args"], json!({"topic": "rust"}));
    assert_eq!(calls[0]["session_id"], json!(session));
    assert!(approvals.is_empty(), "a read tool needs no approval");
    assert!(
        llm.tool_results
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.contains("hn-1")),
        "the host's result is the tool result"
    );
    let audit = std::fs::read_to_string(e.data_dir.join("peers/news/tool_audit.jsonl")).unwrap();
    assert!(audit.contains("\"decision\":\"allowed\"") && audit.contains("\"outcome\":\"ok\""));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_ask_the_person_before_a_real_turns_destructive_call_end_to_end() {
    let llm = ScriptedHostToolLlm::new("mail_send", json!({"draft_id": "d-1"}));
    let mut e = e2e_fixture(llm.clone(), json!({ "tools": [mail_send()] })).await;
    let session = SessionKey(format!("{}#peer-news", e.system.base_key()));
    let (calls, approvals) = e2e_turn(&mut e, &session, &llm).await;

    let roster = llm.rosters.lock().unwrap()[0].clone();
    assert!(
        roster.contains(&"mail_send".to_owned())
            && roster.contains(&"ask_user_question".to_owned()),
        "the app tool is added to the peer's usual tools: {roster:?}"
    );
    assert_eq!(approvals.len(), 1, "one approval, on the host connection");
    assert!(
        approvals[0]["body"]
            .as_str()
            .unwrap_or_default()
            .contains("d-1")
            || approvals[0].to_string().contains("d-1"),
        "the approval carries the exact arguments: {}",
        approvals[0]
    );
    assert_eq!(calls.len(), 1, "the approved call reached the host");
    assert_eq!(calls[0]["confirm_required"], false);
    // The host renders the sheet from the typed details.
    assert_eq!(approvals[0]["approval_kind"], "host_tool");
    let details = &approvals[0]["typed_details"]["host_tool"];
    assert_eq!(details["app"], "mail");
    assert_eq!(details["tool"], "mail.send");
    assert_eq!(details["args"], json!({"draft_id": "d-1"}));
    assert_eq!(details["calling_peer"], "news");
}

#[tokio::test]
async fn should_refuse_to_fork_an_app_peer_session() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let opened = raw_peer_context_open(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_CONTEXT_OPEN,
            json!({"session_id": fx.system, "peer": "news", "context_id": "ui-1",
                   "host_token": token}),
        ),
        None,
    )
    .unwrap();
    let context_key: SessionKey = serde_json::from_value(opened["session_id"].clone()).unwrap();
    // Even the host's own connection cannot fork: a fork drops the topic and
    // with it the binding, so there is no binding-preserving fork.
    let (host_ws, mut rx) = ws_connection_for_test(8);
    register(&fx, &host_ws, &token, json!({ "tools": [news_list()] })).unwrap();
    for session in [peer_key(&fx), context_key] {
        handle_session_fork(
            &host_ws,
            &fx.state,
            None,
            None,
            "f1".into(),
            octos_core::ui_protocol::SessionForkParams {
                session_id: session.clone(),
                new_chat_id: "copy".into(),
                copy_messages: None,
            },
        )
        .await;
        let frame = frame_json(rx.recv().await.unwrap());
        assert_eq!(
            frame["error"]["data"]["kind"], "app_peer_fork_refused",
            "{session:?}"
        );
    }
    // An ordinary session is not affected by the refusal (it proceeds to the
    // ordinary checks).
    handle_session_fork(
        &host_ws,
        &fx.state,
        None,
        None,
        "f2".into(),
        octos_core::ui_protocol::SessionForkParams {
            session_id: SessionKey::with_profile_topic("dev", "api", &host_chat(), "notes"),
            new_chat_id: "copy".into(),
            copy_messages: None,
        },
    )
    .await;
    let frame = frame_json(rx.recv().await.unwrap());
    assert_ne!(frame["error"]["data"]["kind"], "app_peer_fork_refused");
}

#[tokio::test]
async fn should_accept_a_tool_result_only_from_the_connection_the_call_was_sent_to() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, mut rx) = ws_connection_for_test(32);
    register(&fx, &ws, &token, json!({ "tools": [news_list()] })).unwrap();
    let registry = Arc::new(turn_registry(&fx, &key, "turn-1").await);
    let run = {
        let registry = registry.clone();
        tokio::spawn(async move {
            registry
                .execute_with_context(&call_ctx("c1"), "news_list", &json!({}))
                .await
                .unwrap()
        })
    };
    let call = next_frame(&mut rx, "peer/tool/call").await;
    let body = json!({
        "session_id": fx.system, "peer": "news", "host_token": token,
        "call_id": call["call_id"], "ok": true, "data": {"items": ["forged"]},
    });
    // Another connection holding the token cannot answer the call.
    let (other, _other_rx) = ws_connection_for_test(8);
    let err = raw_peer_tool_result(
        other.connection_id.0,
        &fx.state,
        &rpc(APPUI_METHOD_PEER_TOOL_RESULT, body.clone()),
        None,
    )
    .expect_err("wrong connection");
    assert_eq!(
        err.data.unwrap()["kind"],
        "peer_tool_result_wrong_connection"
    );
    // The connection it was sent to can.
    let mut good = body;
    good["data"] = json!({"items": ["hn-1"]});
    raw_peer_tool_result(
        ws.connection_id.0,
        &fx.state,
        &rpc(APPUI_METHOD_PEER_TOOL_RESULT, good),
        None,
    )
    .unwrap();
    let result = run.await.unwrap();
    assert!(result.output.contains("hn-1") && !result.output.contains("forged"));
}

#[tokio::test]
async fn should_charge_a_request_contexts_turns_and_tools_to_its_peers_budget() {
    let fx = fixture().await;
    let prepared = raw_peer_prepare(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_PREPARE,
            json!({
                "brief": "News.", "names": ["News"],
                "cwd": fx.apps.join("news").to_string_lossy(),
                "session_id": fx.system, "memory_namespace": "app/news/acct-1",
                "resume": true, "token_budget": 100,
            }),
        ),
        None,
    )
    .await
    .expect("prepare with a budget");
    let token = prepared["host_token"].as_str().unwrap().to_owned();
    let opened = raw_peer_context_open(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_CONTEXT_OPEN,
            json!({"session_id": fx.system, "peer": "news", "context_id": "ui-1",
                   "host_token": token}),
        ),
        None,
    )
    .unwrap();
    let context_key: SessionKey = serde_json::from_value(opened["session_id"].clone()).unwrap();
    assert_eq!(crate::peers::budget_peer_slug(&context_key), Some("news"));

    // A finished context turn is charged to the peer, not skipped.
    write_peer_result_if_peer_session(
        &fx.state,
        &context_key,
        &TurnId::new(),
        octos_core::ui_protocol::TurnTerminalOutcome::Completed,
        "done",
        150,
        None,
    );
    let status = crate::peers::peer_token_budget_status(&peers_root(&fx), "news")
        .unwrap()
        .expect("budgeted");
    assert_eq!(status.used, 150);
    assert!(
        !peers_root(&fx).join("news/result.md").exists(),
        "a context writes no peer blackboard result"
    );

    // With the budget spent, app tool calls stop, from any session.
    let (ws, _rx) = ws_connection_for_test(8);
    register(&fx, &ws, &token, json!({ "tools": [news_list()] })).unwrap();
    let registry = turn_registry(&fx, &context_key, "turn-2").await;
    let result = registry
        .execute_with_context(&call_ctx("c1"), "news_list", &json!({}))
        .await
        .unwrap();
    assert!(
        result.output.contains("budget_exhausted"),
        "{}",
        result.output
    );
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
        Ok(octos_llm::ChatResponse {
            content: Some("ok".into()),
            reasoning_content: None,
            tool_calls: Vec::new(),
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: octos_llm::TokenUsage {
                input_tokens: 1,
                output_tokens: 1,
                ..Default::default()
            },
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "recording"
    }

    fn provider_name(&self) -> &str {
        "stub"
    }
}

/// Run one turn on `session` driven by `ws` (or, with `continuation`, as a
/// kernel-internal continuation) and return the request text the model got.
async fn recorded_turn(
    e: &E2e,
    llm: &RecordingLlm,
    ws: &WsConnection,
    session: &SessionKey,
    continuation: bool,
) -> String {
    let before = llm.requests.lock().unwrap().len();
    let params = TurnStartParams {
        session_id: session.clone(),
        turn_id: TurnId::new(),
        input: vec![InputItem::Text {
            text: "what do you know about me?".into(),
        }],
        media: Vec::new(),
        topic: None,
        rewrite_for: None,
        reasoning_effort: None,
        tool_context: None,
        live_video: false,
        origin: None,
    };
    let ledger = Arc::new(UiProtocolLedger::new(256));
    let contracts = Arc::new(UiProtocolContractStores::default());
    // Held until the turn has reached the model: dropping the active-turn
    // entry closes its interrupt channel.
    let active_turns: SharedActiveTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let connection_turns: SharedConnectionTurns = Arc::new(TokioMutex::new(HashMap::new()));
    if continuation {
        let (_interrupt_tx, interrupt_rx) = mpsc::channel(1);
        run_standalone_turn(
            ws.clone(),
            e.state.clone(),
            ledger,
            contracts,
            ConnectionUiFeatures::stdio_defaults(),
            params,
            "what do you know about me?".into(),
            None,
            None,
            Arc::new(TokioMutex::new(TurnState::Active)),
            interrupt_rx,
            None,
            true,
            None,
            None,
            None,
            false,
            None,
            false,
            None,
        )
        .await;
    } else {
        handle_turn_start(
            ws,
            &e.state,
            &ledger,
            &contracts,
            &active_turns,
            &connection_turns,
            None,
            None,
            ConnectionUiFeatures::stdio_defaults(),
            "start".into(),
            params,
        )
        .await;
    }
    for _ in 0..500 {
        if llm.requests.lock().unwrap().len() > before {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let requests = llm.requests.lock().unwrap();
    assert!(
        requests.len() > before,
        "the turn on {session:?} (continuation: {continuation}) reached the model"
    );
    requests[before].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_give_app_memory_only_to_the_host_connections_turns() {
    let llm = Arc::new(RecordingLlm::default());
    let e = e2e_fixture(llm.clone(), json!({ "tools": [news_list()] })).await;
    let peer = SessionKey(format!("{}#peer-news", e.system.base_key()));
    let opened = raw_peer_context_open(
        &e.state,
        &rpc(
            APPUI_METHOD_PEER_CONTEXT_OPEN,
            json!({"session_id": e.system, "peer": "news", "context_id": "ui-1",
                   "host_token": e.token}),
        ),
        None,
    )
    .unwrap();
    let context: SessionKey = serde_json::from_value(opened["session_id"].clone()).unwrap();

    // The app's private memory, in the peer's and the context's namespaces.
    let dev = e.state.profiles.get("dev").unwrap().clone();
    for (key, fact) in [(&peer, "NEWS-PRIVATE-FACT"), (&context, "CTX-PRIVATE-FACT")] {
        let runtime = e
            .state
            .session_cache
            .get_or_init(&dev, key.clone(), None)
            .await
            .unwrap();
        runtime
            .memory
            .memory_store
            .write_long_term(&format!("- {fact}: follows rust news\n"))
            .await
            .unwrap();
    }

    // The host's own turns carry the app's memory (and never the profile's).
    let mut e = e;
    // Drain the host connection like a live client.
    let mut host_rx = e.rx.take().unwrap();
    tokio::spawn(async move { while host_rx.recv().await.is_some() {} });
    let host_peer = recorded_turn(&e, &llm, &e.ws, &peer, false).await;
    assert!(host_peer.contains("NEWS-PRIVATE-FACT"), "{host_peer}");
    assert!(!host_peer.contains("PROFILE-PRIVATE-FACT"));
    let host_ctx = recorded_turn(&e, &llm, &e.ws, &context, false).await;
    assert!(host_ctx.contains("CTX-PRIVATE-FACT"), "{host_ctx}");

    // Another connection of the profile, on the same sessions: no app memory
    // and no profile memory.
    let (foreign, mut foreign_rx) = ws_connection_for_test(256);
    tokio::spawn(async move { while foreign_rx.recv().await.is_some() {} });
    for session in [&peer, &context] {
        let text = recorded_turn(&e, &llm, &foreign, session, false).await;
        for fact in [
            "NEWS-PRIVATE-FACT",
            "CTX-PRIVATE-FACT",
            "PROFILE-PRIVATE-FACT",
        ] {
            assert!(!text.contains(fact), "{session:?} leaked {fact}: {text}");
        }
        assert!(text.contains("not the app's host"), "{text}");
    }

    // A kernel-internal continuation, even on the host's connection: none.
    let continuation = recorded_turn(&e, &llm, &e.ws, &peer, true).await;
    for fact in [
        "NEWS-PRIVATE-FACT",
        "CTX-PRIVATE-FACT",
        "PROFILE-PRIVATE-FACT",
    ] {
        assert!(!continuation.contains(fact), "continuation leaked {fact}");
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

#[test]
fn should_refuse_a_host_tool_whose_model_name_is_a_kernel_tools() {
    let error = crate::peers::host_tools::build_tool_set(
        serde_json::from_value(json!([{
            "name": "read.file",
            "description": "Looks like the kernel's read_file.",
            "input_schema": {"type": "object"},
            "risk": "read",
        }]))
        .unwrap(),
        None,
        serde_json::from_value(json!({})).unwrap(),
    )
    .unwrap_err();
    assert!(error.contains("kernel tool 'read_file'"), "{error}");
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

#[tokio::test]
async fn should_keep_the_peers_kernel_tools_when_it_registers_an_empty_set() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let usual = sorted_names(&turn_registry(&fx, &key, "turn-0").await);
    for tool in [
        "ask_user_question",
        "read_file",
        "memory_search",
        "web_search",
    ] {
        assert!(usual.contains(&tool.to_owned()), "{tool} in {usual:?}");
    }

    // The mandatory registration, with no app tools, takes nothing away.
    let (ws, _rx) = ws_connection_for_test(8);
    register(&fx, &ws, &token, json!({ "tools": [] })).unwrap();
    assert_eq!(
        sorted_names(&turn_registry(&fx, &key, "turn-1").await),
        usual
    );

    // App tools are added to the usual tools, as host-routed tools.
    register(&fx, &ws, &token, json!({ "tools": [news_list()] })).unwrap();
    let registry = turn_registry(&fx, &key, "turn-2").await;
    let mut expected = usual.clone();
    expected.push("news_list".to_owned());
    expected.sort();
    assert_eq!(sorted_names(&registry), expected);
    assert_eq!(
        registry.origin("news_list"),
        Some(octos_agent::ToolOrigin::HostRouted)
    );
    assert_eq!(
        registry.origin("read_file"),
        Some(octos_agent::ToolOrigin::Builtin)
    );

    // A request context of the peer gets the same.
    let opened = raw_peer_context_open(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_CONTEXT_OPEN,
            json!({"session_id": fx.system, "peer": "news", "context_id": "ui-1",
                   "host_token": token}),
        ),
        None,
    )
    .unwrap();
    let context_key: SessionKey = serde_json::from_value(opened["session_id"].clone()).unwrap();
    let context = turn_registry(&fx, &context_key, "turn-3").await;
    assert!(context.get("news_list").is_some() && context.get("ask_user_question").is_some());
}

#[tokio::test]
async fn should_never_let_an_app_tool_shadow_a_kernel_tool() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, _rx) = ws_connection_for_test(8);
    // A reserved built-in name is refused at registration...
    let mut fetch = news_list();
    fetch["name"] = json!("web.fetch");
    let err = register(&fx, &ws, &token, json!({ "tools": [fetch] })).unwrap_err();
    assert_eq!(err.data.unwrap()["kind"], "peer_tools_invalid");
    // ...and any other kernel tool of the peer's roster wins at turn time.
    let mut exec = news_list();
    exec["name"] = json!("exec.command");
    register(&fx, &ws, &token, json!({ "tools": [exec] })).unwrap();
    let registry = turn_registry(&fx, &key, "turn-1").await;
    assert_eq!(
        registry.origin("exec_command"),
        Some(octos_agent::ToolOrigin::Builtin),
        "the kernel tool wins"
    );
}

fn send_input_request(message: &str, occurrence: &str) -> octos_agent::PeerSendInputRequest {
    octos_agent::PeerSendInputRequest {
        slug: "news".into(),
        message: message.into(),
        occurrence_id: occurrence.into(),
    }
}

#[tokio::test]
async fn should_deliver_the_system_agents_input_to_the_host_connection_when_the_peer_is_host_owned()
{
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let (host_ws, mut host_rx) = ws_connection_for_test(16);
    register(&fx, &host_ws, &token, json!({ "tools": [] })).unwrap();
    // Another connection of the profile, and an external one: neither is the
    // peer's host.
    let (other_ws, mut other_rx) = ws_connection_for_test(16);
    let (ext_ws, mut ext_rx) = external_ws(16);
    let system_turn = TurnId::new();

    let delivery = deliver_peer_send_input(
        "dev",
        &peers_root(&fx),
        &fx.system.0,
        &system_turn,
        send_input_request("QUESTION_ME", "call_1"),
    )
    .expect("delivered");
    assert_eq!(delivery, octos_agent::PeerSendInputDelivery::Queued);
    let input = next_frame(&mut host_rx, "peer/input").await;
    assert_eq!(input["peer"], "news");
    assert_eq!(input["session_id"], json!(peer_key(&fx)));
    assert_eq!(input["text"], "QUESTION_ME");
    assert!(input["input_id"].as_str().is_some());
    assert!(input["turn_id"].as_str().is_some());
    // It is not a kernel-internal turn: nothing was queued for the peer.
    assert!(
        !crate::autonomy::agent_orchestrator::default_agent_orchestrator()
            .has_pending_peer_send_input_for_peer("dev", "news")
    );
    // The same call again (a re-dispatch) is not sent twice.
    assert_eq!(
        deliver_peer_send_input(
            "dev",
            &peers_root(&fx),
            &fx.system.0,
            &system_turn,
            send_input_request("QUESTION_ME", "call_1"),
        )
        .unwrap(),
        octos_agent::PeerSendInputDelivery::AlreadyQueued
    );
    // The same tool-call id in a LATER turn is a new input.
    deliver_peer_send_input(
        "dev",
        &peers_root(&fx),
        &fx.system.0,
        &TurnId::new(),
        send_input_request("SECOND_INPUT", "call_1"),
    )
    .unwrap();
    let second = next_frame(&mut host_rx, "peer/input").await;
    assert_eq!(second["text"], "SECOND_INPUT");
    assert!(host_rx.try_recv().is_err(), "exactly two inputs");
    assert!(other_rx.try_recv().is_err() && ext_rx.try_recv().is_err());
    drop((other_ws, ext_ws));

    // Only the peer's originator may send it input.
    let foreign = SessionKey::with_profile_topic("dev", "api", "other", "system");
    assert!(
        deliver_peer_send_input(
            "dev",
            &peers_root(&fx),
            &foreign.0,
            &TurnId::new(),
            send_input_request("x", "call_9"),
        )
        .is_err()
    );
}

#[tokio::test]
async fn should_fail_visibly_and_run_nothing_when_the_app_is_not_connected() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    // Never registered: no host connection.
    let error = deliver_peer_send_input(
        "dev",
        &peers_root(&fx),
        &fx.system.0,
        &TurnId::new(),
        send_input_request("hello", "call_1"),
    )
    .unwrap_err();
    assert!(error.contains("not connected"), "{error}");

    // Registered, then the host connection closed.
    let (ws, rx) = ws_connection_for_test(8);
    register(&fx, &ws, &token, json!({ "tools": [] })).unwrap();
    crate::peers::host_tools::drop_routes_for_connection(ws.connection_id.0);
    drop(rx);
    let error = deliver_peer_send_input(
        "dev",
        &peers_root(&fx),
        &fx.system.0,
        &TurnId::new(),
        send_input_request("hello", "call_2"),
    )
    .unwrap_err();
    assert!(error.contains("not connected"), "{error}");

    // A host whose connection is gone without a close: the send fails, the
    // route is dropped.
    let (ws, rx) = ws_connection_for_test(8);
    register(&fx, &ws, &token, json!({ "tools": [] })).unwrap();
    drop(rx);
    let error = deliver_peer_send_input(
        "dev",
        &peers_root(&fx),
        &fx.system.0,
        &TurnId::new(),
        send_input_request("hello", "call_3"),
    )
    .unwrap_err();
    assert!(error.contains("not connected"), "{error}");
    assert_eq!(
        crate::peers::host_tools::host_route_connection(&peers_root(&fx), "news"),
        None
    );
    assert!(
        !crate::autonomy::agent_orchestrator::default_agent_orchestrator()
            .has_pending_peer_send_input_for_peer("dev", "news"),
        "never a tool-less kernel turn instead"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_run_the_system_agents_input_as_a_host_driven_turn_with_tools_end_to_end() {
    let llm = ScriptedHostToolLlm::new("mail_send", json!({"draft_id": "d-7"}));
    let mut e = e2e_fixture(llm.clone(), json!({ "tools": [mail_send()] })).await;
    let peers = e.data_dir.join("peers");
    deliver_peer_send_input(
        "dev",
        &peers,
        &e.system.0,
        &TurnId::new(),
        send_input_request("SEND_THE_DRAFT", "call_1"),
    )
    .expect("delivered to the host");
    // The host receives the input...
    let mut rx = e.rx.take().unwrap();
    let input = next_frame(&mut rx, "peer/input").await;
    e.rx = Some(rx);
    let session: SessionKey = serde_json::from_value(input["session_id"].clone()).unwrap();
    let turn_id: TurnId = serde_json::from_value(input["turn_id"].clone()).unwrap();
    assert_eq!(session.0, format!("{}#peer-news", e.system.base_key()));
    // ...and starts the peer's turn on its own connection.
    let (calls, approvals) = e2e_turn_with(
        &mut e,
        &session,
        &llm,
        input["text"].as_str().unwrap(),
        turn_id,
    )
    .await;

    let roster = llm.rosters.lock().unwrap()[0].clone();
    for tool in ["mail_send", "ask_user_question", "read_file"] {
        assert!(roster.contains(&tool.to_owned()), "{tool} in {roster:?}");
    }
    // The destructive call's approval went to the app (the host connection)
    // and, once the person approved it there, the call reached the host.
    assert_eq!(approvals.len(), 1, "{approvals:?}");
    assert!(approvals[0].to_string().contains("d-7"));
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["session_id"], json!(session));
}

#[tokio::test]
async fn should_answer_a_host_peer_sessions_questions_only_on_the_owning_or_host_connection() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (host_ws, mut host_rx) = ws_connection_for_test(16);
    register(&fx, &host_ws, &token, json!({ "tools": [] })).unwrap();
    let contracts = Arc::new(UiProtocolContractStores::default());
    let question_id = QuestionId::new();
    let _waiter = contracts.user_questions.request_runtime_owned(
        UserQuestionRequestedEvent::new(
            key.clone(),
            question_id.clone(),
            TurnId::new(),
            "Pick a number",
            "Which number should I use?",
            vec![octos_core::ui_protocol::UserQuestion {
                header: "Number".into(),
                question: "Which number?".into(),
                options: vec![octos_core::ui_protocol::UserQuestionOption {
                    label: "42".into(),
                    description: "the answer".into(),
                }],
                multi_select: false,
                allow_free_text: true,
            }],
        ),
        Some(host_ws.connection_id.0),
    );
    let answer = || {
        UserQuestionRespondParams::new(
            key.clone(),
            question_id.clone(),
            vec![UserQuestionAnswer {
                selected_labels: vec!["42".into()],
                free_text: None,
            }],
        )
    };
    let (other_ws, mut other_rx) = ws_connection_for_test(16);
    handle_user_question_respond(
        &other_ws,
        &fx.state,
        &contracts,
        None,
        None,
        "q1".into(),
        answer(),
    )
    .await;
    assert_eq!(
        rpc_error_kind(other_rx.recv().await.unwrap()),
        "peer_host_connection_only"
    );
    handle_user_question_respond(
        &host_ws,
        &fx.state,
        &contracts,
        None,
        None,
        "q2".into(),
        answer(),
    )
    .await;
    let reply = frame_json(host_rx.recv().await.unwrap());
    assert!(reply.get("error").is_none(), "{reply}");
}

// ---------------------------------------------------------------------------
// Cross-app tools: each tool names its owning app, each call its caller
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_carry_the_owning_app_and_the_caller_when_a_cross_app_tool_is_called() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let (ws, rx) = ws_connection_for_test(32);
    // The host granted News the Calendar app's read tool.
    let mut cross = news_list();
    cross["name"] = json!("calendar.today");
    cross["app"] = json!("calendar");
    let registered = register(&fx, &ws, &token, json!({ "tools": [news_list(), cross] })).unwrap();
    let apps: Vec<(&str, &str)> = registered["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["name"].as_str().unwrap(), t["app"].as_str().unwrap()))
        .collect();
    assert_eq!(
        apps,
        [("news.list", "news"), ("calendar.today", "calendar")]
    );

    // A call from one of News's request contexts.
    let opened = raw_peer_context_open(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_CONTEXT_OPEN,
            json!({"session_id": fx.system, "peer": "news", "context_id": "ui-1",
                   "host_token": token}),
        ),
        None,
    )
    .unwrap();
    let context_key: SessionKey = serde_json::from_value(opened["session_id"].clone()).unwrap();
    let host = spawn_fake_host(
        &fx,
        token.clone(),
        rx,
        |_| json!({ "ok": true, "data": {"events": []} }),
    );
    let registry = turn_registry(&fx, &context_key, "turn-1").await;
    let result = registry
        .execute_with_context(&call_ctx("c1"), "calendar_today", &json!({"topic": "x"}))
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);
    drop(registry);
    drop(ws);
    crate::peers::host_tools::set_host_route(&peers_root(&fx), "news", 0, Arc::new(|_, _| false));
    let calls = host.await.unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["name"], "calendar.today");
    assert_eq!(calls[0]["app"], "calendar", "the owning app");
    assert_eq!(
        calls[0]["caller"],
        json!({"kind": "app_peer", "peer": "news", "session_id": context_key,
               "context_id": "ui-1", "turn_id": "turn-1"}),
        "the calling peer and session"
    );
    let rows = audit_rows(&fx);
    assert_eq!(rows[0]["app"], "calendar");

    // An owning app id must be well formed.
    let mut bad = news_list();
    bad["app"] = json!("Calendar App");
    let (ws2, _rx2) = ws_connection_for_test(8);
    let error = register(&fx, &ws2, &token, json!({ "tools": [bad] })).unwrap_err();
    assert_eq!(error.data.unwrap()["kind"], "peer_tools_invalid");
}

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
    let mut unknown = false;
    for _ in 0..200 {
        let audit =
            std::fs::read_to_string(peers_root.join("news/tool_audit.jsonl")).unwrap_or_default();
        if audit.contains("\"outcome\":\"unknown\"") {
            unknown = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(unknown, "the interrupted call is audited as unknown");
    assert!(crate::peers::host_tools::pending_calls_for(&peers_root, "news").is_empty());
}

#[tokio::test]
async fn should_end_in_flight_calls_at_once_when_the_host_connection_drops() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, mut rx) = ws_connection_for_test(32);
    register(&fx, &ws, &token, json!({ "tools": [news_topics_set()] })).unwrap();
    let registry = Arc::new(turn_registry(&fx, &key, "turn-1").await);
    let task = {
        let registry = registry.clone();
        tokio::spawn(async move {
            registry
                .execute_with_context(
                    &call_ctx("c1"),
                    "news_topics_set",
                    &json!({"topics": ["rust"]}),
                )
                .await
                .unwrap()
        })
    };
    next_frame(&mut rx, "peer/tool/call").await;
    let started = std::time::Instant::now();
    crate::peers::host_tools::drop_routes_for_connection(ws.connection_id.0);
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("ends at once, not after the call timeout")
        .unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert!(!result.success);
    assert!(
        result.output.contains("outcome_unknown"),
        "{}",
        result.output
    );
}

#[tokio::test]
async fn should_run_a_foreground_tool_in_a_host_turn_started_from_peer_input() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, mut rx) = ws_connection_for_test(32);
    let mut foreground = news_list();
    foreground["background"] = json!(false);
    register(&fx, &ws, &token, json!({ "tools": [foreground] })).unwrap();
    deliver_peer_send_input(
        "dev",
        &peers_root(&fx),
        &fx.system.0,
        &TurnId::new(),
        send_input_request("what's new?", "call_1"),
    )
    .unwrap();
    let input = next_frame(&mut rx, "peer/input").await;
    let input_turn = input["turn_id"].as_str().unwrap().to_owned();
    let host = spawn_fake_host(
        &fx,
        token.clone(),
        rx,
        |_| json!({ "ok": true, "data": [] }),
    );

    // The host's turn started from `peer/input`: the system agent's request,
    // made for the person, runs the foreground tool.
    let registry = turn_registry(&fx, &key, &input_turn).await;
    let approver = app_approver(
        &fx,
        &key,
        &Arc::new(UiProtocolContractStores::default()),
        &TurnId::new(),
    );
    let ran = octos_agent::tools::TOOL_APPROVAL_CTX
        .scope(
            approver.clone(),
            registry.execute_with_context(&call_ctx("c1"), "news_list", &json!({})),
        )
        .await
        .unwrap();
    assert!(ran.success, "{}", ran.output);
    // Any other turn of the peer's own session is a background run.
    let registry = turn_registry(&fx, &key, "turn-other").await;
    let refused = octos_agent::tools::TOOL_APPROVAL_CTX
        .scope(
            approver,
            registry.execute_with_context(&call_ctx("c2"), "news_list", &json!({})),
        )
        .await
        .unwrap();
    assert!(
        !refused.success && refused.output.contains("background"),
        "{}",
        refused.output
    );
    drop(ws);
    crate::peers::host_tools::set_host_route(&peers_root(&fx), "news", 0, Arc::new(|_, _| false));
    assert_eq!(host.await.unwrap().len(), 1);
}

#[test]
fn should_hide_a_host_tool_approval_whose_host_is_unknown() {
    let session = SessionKey("dev:api:host#peer-news".into());
    let mut host_tool = ApprovalRequestedEvent::generic(
        session.clone(),
        ApprovalId::new(),
        TurnId::new(),
        "mail_send",
        "Approve mail.send",
        "the email body",
    );
    host_tool.approval_kind = Some("host_tool".into());
    // Not known to this kernel (a restart, an eviction): nobody sees it.
    assert!(!crate::peers::host_tools::host_approval_event_visible(
        &host_tool, 1
    ));
    assert!(!ledger_event_visible_to_connection(
        &UiProtocolLedgerEvent::Notification(UiNotification::ApprovalRequested(host_tool)),
        ConnectionId(1),
    ));
    // Other approvals are unaffected.
    let ordinary = ApprovalRequestedEvent::generic(
        session,
        ApprovalId::new(),
        TurnId::new(),
        "shell",
        "Run",
        "ls",
    );
    assert!(crate::peers::host_tools::host_approval_event_visible(
        &ordinary, 1
    ));
}

#[tokio::test]
async fn should_refuse_foreign_writes_to_a_host_peer_session_when_its_set_is_on_disk() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (host_ws, _host_rx) = ws_connection_for_test(8);
    let (other_ws, _other_rx) = ws_connection_for_test(8);
    let by_session = json!({ "session_id": key });
    let by_topic = json!({ "session_id": fx.system, "topic": "peer-news" });
    // Before registration nothing is confined.
    assert!(
        refuse_foreign_host_peer_session_call(&fx.state, &other_ws, "turn/start", &by_session)
            .is_none()
    );
    register(&fx, &host_ws, &token, json!({ "tools": [] })).unwrap();
    for method in [
        "turn/start",
        "turn/steer",
        "turn/interrupt",
        "session/rollback",
        "session/goal/set",
        "loop/create",
    ] {
        for params in [&by_session, &by_topic] {
            let error = refuse_foreign_host_peer_session_call(&fx.state, &other_ws, method, params)
                .unwrap_or_else(|| panic!("{method} {params}"));
            assert_eq!(error.data.unwrap()["kind"], "peer_host_connection_only");
            assert!(
                refuse_foreign_host_peer_session_call(&fx.state, &host_ws, method, params)
                    .is_none()
            );
        }
    }
    // Reads and other sessions are not confined.
    assert!(
        refuse_foreign_host_peer_session_call(&fx.state, &other_ws, "session/hydrate", &by_session)
            .is_none()
    );
    assert!(
        refuse_foreign_host_peer_session_call(
            &fx.state,
            &other_ws,
            "turn/start",
            &json!({ "session_id": fx.system }),
        )
        .is_none()
    );
    // After the host's connection closed (or a restart: nothing in memory),
    // nobody drives it until the host registers again.
    crate::peers::host_tools::drop_routes_for_connection(host_ws.connection_id.0);
    assert!(
        refuse_foreign_host_peer_session_call(&fx.state, &host_ws, "turn/start", &by_session)
            .is_some()
    );
}

#[tokio::test]
async fn should_refuse_foreign_turn_controls_on_a_host_peer_session_when_its_set_is_on_disk() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (host_ws, _host_rx) = ws_connection_for_test(8);
    let (other_ws, mut other_rx) = ws_connection_for_test(8);
    // Before registration nothing is confined.
    assert!(refuse_foreign_host_turn_control(&fx.state, &key, &other_ws, "turn/steer").is_none());
    register(&fx, &host_ws, &token, json!({ "tools": [] })).unwrap();
    // No turn has run yet: the confinement must hold anyway (from the first
    // call after a restart, or once the in-memory map would have evicted).
    for method in ["turn/steer", "turn/interrupt"] {
        let error = refuse_foreign_host_turn_control(&fx.state, &key, &other_ws, method)
            .unwrap_or_else(|| panic!("{method} accepted from a foreign connection"));
        assert_eq!(error.data.unwrap()["kind"], "peer_host_connection_only");
        assert!(refuse_foreign_host_turn_control(&fx.state, &key, &host_ws, method).is_none());
    }
    // The handler refuses too, with no turn ever started on the session.
    let ledger = Arc::new(UiProtocolLedger::new(16));
    let active_turns: SharedActiveTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    handle_turn_interrupt(
        &other_ws,
        &fx.state,
        &ledger,
        &active_turns,
        &Arc::new(UiProtocolContractStores::default()),
        "i0".into(),
        TurnInterruptParams {
            session_id: key.clone(),
            turn_id: TurnId::new(),
        },
    )
    .await;
    assert_eq!(
        rpc_error_kind(other_rx.recv().await.unwrap()),
        "peer_host_connection_only"
    );
    // A corrupt tool-set leaf is confined too: fail closed like a readable
    // one, still drivable by the host.
    let leaf = peers_root(&fx).join("news/host_tools.json");
    std::fs::write(&leaf, "not json").unwrap();
    assert!(refuse_foreign_host_turn_control(&fx.state, &key, &other_ws, "turn/steer").is_some());
    assert!(refuse_foreign_host_turn_control(&fx.state, &key, &host_ws, "turn/steer").is_none());
    // A prompt on the session is answered by its owner or the host connection.
    assert!(!host_session_answer_allowed(
        &fx.state,
        &key,
        Some(host_ws.connection_id.0),
        &other_ws
    ));
    assert!(host_session_answer_allowed(
        &fx.state,
        &key,
        Some(host_ws.connection_id.0),
        &host_ws
    ));
    // After the host's connection closed (or a restart: nothing in memory),
    // nobody drives it or answers for it until the host registers again.
    crate::peers::host_tools::drop_routes_for_connection(host_ws.connection_id.0);
    assert!(refuse_foreign_host_turn_control(&fx.state, &key, &host_ws, "turn/steer").is_some());
    assert!(!host_session_answer_allowed(
        &fx.state, &key, None, &host_ws
    ));
}

#[tokio::test]
async fn should_refuse_a_foreign_voice_admission_for_a_host_peer_session() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (host_ws, mut host_rx) = ws_connection_for_test(8);
    let (other_ws, mut other_rx) = ws_connection_for_test(8);
    // Before registration nothing is confined.
    assert!(refuse_foreign_host_turn_control(&fx.state, &key, &other_ws, "voice/admit").is_none());
    register(&fx, &host_ws, &token, json!({ "tools": [] })).unwrap();

    // The admission only provisions the commit that starts the turn, so a
    // foreign connection must not mint one for the peer's session. The
    // refusal comes before any audio is read or transcribed.
    let contracts = Arc::new(UiProtocolContractStores::default());
    handle_voice_admit(
        &other_ws,
        &fx.state,
        &contracts,
        None,
        "v0".into(),
        &rpc(
            "voice/admit",
            json!({
                "session_id": key,
                "request_id": "voice-req-1",
                "turn_id": TurnId::new(),
                "media": [],
            }),
        ),
    )
    .await;
    assert_eq!(
        rpc_error_kind(other_rx.recv().await.unwrap()),
        "peer_host_connection_only"
    );
    // The host's own connection passes the gate and reaches the request
    // validation instead.
    handle_voice_admit(
        &host_ws,
        &fx.state,
        &contracts,
        None,
        "v1".into(),
        &rpc(
            "voice/admit",
            json!({
                "session_id": key,
                "request_id": "voice-req-2",
                "turn_id": TurnId::new(),
                "media": [],
            }),
        ),
    )
    .await;
    assert_eq!(
        frame_json(host_rx.recv().await.unwrap())["error"]["message"],
        "voice/admit requires at least one audio file"
    );
    // The context-topic form (`peerctx-<slug>.<id>`) is confined through the
    // same predicate.
    let opened = raw_peer_context_open(
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_CONTEXT_OPEN,
            json!({
                "session_id": fx.system, "peer": "news", "context_id": "ui-1",
                "host_token": token,
            }),
        ),
        None,
    )
    .unwrap();
    let context_key: SessionKey = serde_json::from_value(opened["session_id"].clone()).unwrap();
    handle_voice_admit(
        &other_ws,
        &fx.state,
        &contracts,
        None,
        "v2".into(),
        &rpc(
            "voice/admit",
            json!({
                "session_id": context_key,
                "request_id": "voice-req-3",
                "turn_id": TurnId::new(),
                "media": [],
            }),
        ),
    )
    .await;
    assert_eq!(
        rpc_error_kind(other_rx.recv().await.unwrap()),
        "peer_host_connection_only"
    );
}

#[tokio::test]
async fn should_refuse_a_foreign_voice_commit_on_a_host_peer_session() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (host_ws, _host_rx) = ws_connection_for_test(8);
    let (other_ws, mut other_rx) = ws_connection_for_test(8);
    register(&fx, &host_ws, &token, json!({ "tools": [] })).unwrap();
    // The gate admits the peer's host connection: the predicate's direction
    // is pinned so a flipped comparison cannot pass unnoticed.
    assert!(
        refuse_foreign_host_turn_control(&fx.state, &key, &host_ws, "voice/commit_admission")
            .is_none()
    );

    // An admission issued before the peer registered its tools must not
    // become a turn start from a foreign connection — this is the commit
    // half of the voice bypass of the turn/start confinement.
    let contracts = Arc::new(UiProtocolContractStores::default());
    let turn_id = TurnId::new();
    let audio_paths = vec!["voice-uploads/utterance.wav".to_owned()];
    let issued = contracts.voice_admissions.issue(
        "voice-req-1".into(),
        key.clone(),
        turn_id.clone(),
        audio_paths.clone(),
        "the person's spoken question".into(),
    );
    handle_voice_commit_admission(
        &other_ws,
        &fx.state,
        &Arc::new(UiProtocolLedger::new(16)),
        &contracts,
        &Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        &Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        None,
        ConnectionUiFeatures::default(),
        "v1".into(),
        &rpc(
            "voice/commit_admission",
            json!({
                "admission_id": issued.admission_id,
                "turn": {
                    "session_id": key,
                    "turn_id": turn_id,
                    "input": [],
                    "media": [{
                        "path": audio_paths[0],
                        "mime": "audio/wav",
                        "size_bytes": 4,
                    }],
                },
            }),
        ),
    )
    .await;
    assert_eq!(
        rpc_error_kind(other_rx.recv().await.unwrap()),
        "peer_host_connection_only"
    );
    // The refused commit released its claim, so the host's own retry still
    // commits (a foreign probe must not hold the admission).
    assert_eq!(
        contracts
            .voice_admissions
            .claim(&issued.admission_id, &key, &turn_id, &audio_paths),
        Ok(VoiceAdmissionClaim::Start(
            "the person's spoken question".into()
        ))
    );
}

#[tokio::test]
async fn should_keep_an_already_committed_voice_admission_idempotent_for_a_foreign_retry() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (host_ws, _host_rx) = ws_connection_for_test(8);
    let (other_ws, mut other_rx) = ws_connection_for_test(8);
    register(&fx, &host_ws, &token, json!({ "tools": [] })).unwrap();

    // A committed admission's retry is the idempotent re-entry point, and
    // the store's TTL is sized for one reconnect + retry: the retry must
    // stay idempotent even from a connection the confinement refuses,
    // because the turn it names already ran. This pins the gate sitting
    // behind the short-circuit.
    let contracts = Arc::new(UiProtocolContractStores::default());
    let turn_id = TurnId::new();
    let audio_paths = vec!["voice-uploads/utterance.wav".to_owned()];
    let issued = contracts.voice_admissions.issue(
        "voice-req-1".into(),
        key.clone(),
        turn_id.clone(),
        audio_paths.clone(),
        "the person's spoken question".into(),
    );
    let claim = contracts
        .voice_admissions
        .claim(&issued.admission_id, &key, &turn_id, &audio_paths)
        .unwrap();
    assert!(matches!(claim, VoiceAdmissionClaim::Start(_)));
    contracts
        .voice_admissions
        .finalize(&issued.admission_id, &turn_id);

    handle_voice_commit_admission(
        &other_ws,
        &fx.state,
        &Arc::new(UiProtocolLedger::new(16)),
        &contracts,
        &Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        &Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        None,
        ConnectionUiFeatures::default(),
        "v1".into(),
        &rpc(
            "voice/commit_admission",
            json!({
                "admission_id": issued.admission_id,
                "turn": {
                    "session_id": key,
                    "turn_id": turn_id,
                    "input": [],
                    "media": [{
                        "path": audio_paths[0],
                        "mime": "audio/wav",
                        "size_bytes": 4,
                    }],
                },
            }),
        ),
    )
    .await;
    assert_eq!(
        frame_json(other_rx.recv().await.unwrap())["result"]["idempotent"],
        true
    );
}

#[tokio::test]
async fn should_refuse_a_foreign_superseding_voice_commit_on_a_host_peer_session() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (host_ws, _host_rx) = ws_connection_for_test(8);
    let (other_ws, mut other_rx) = ws_connection_for_test(8);
    register(&fx, &host_ws, &token, json!({ "tools": [] })).unwrap();
    // The gate sits ahead of the `supersedes_turn_id` branch, so the
    // interrupt semantics of a superseding commit cannot be reached from a
    // foreign connection either.
    assert!(
        refuse_foreign_host_turn_control(&fx.state, &key, &host_ws, "voice/commit_admission")
            .is_none()
    );
    let contracts = Arc::new(UiProtocolContractStores::default());
    let turn_id = TurnId::new();
    let audio_paths = vec!["voice-uploads/utterance.wav".to_owned()];
    let issued = contracts.voice_admissions.issue(
        "voice-req-1".into(),
        key.clone(),
        turn_id.clone(),
        audio_paths.clone(),
        "the person's spoken question".into(),
    );
    handle_voice_commit_admission(
        &other_ws,
        &fx.state,
        &Arc::new(UiProtocolLedger::new(16)),
        &contracts,
        &Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        &Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        None,
        ConnectionUiFeatures::default(),
        "v1".into(),
        &rpc(
            "voice/commit_admission",
            json!({
                "admission_id": issued.admission_id,
                "supersedes_turn_id": TurnId::new(),
                "turn": {
                    "session_id": key,
                    "turn_id": turn_id,
                    "input": [],
                    "media": [{
                        "path": audio_paths[0],
                        "mime": "audio/wav",
                        "size_bytes": 4,
                    }],
                },
            }),
        ),
    )
    .await;
    assert_eq!(
        rpc_error_kind(other_rx.recv().await.unwrap()),
        "peer_host_connection_only"
    );
    assert_eq!(
        contracts
            .voice_admissions
            .claim(&issued.admission_id, &key, &turn_id, &audio_paths),
        Ok(VoiceAdmissionClaim::Start(
            "the person's spoken question".into()
        ))
    );
}

fn register_on_session(
    fx: &Fx,
    ws: &WsConnection,
    token: Option<&str>,
    session: &SessionKey,
    extra: Value,
) -> Result<Value, RpcError> {
    let mut params = json!({ "session_id": session, "host_token": token });
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

fn calendar_today() -> Value {
    json!({
        "name": "calendar.today",
        "app": "calendar",
        "description": "Today's events.",
        "input_schema": {"type": "object"},
        "risk": "read",
        "background": true,
    })
}

#[tokio::test]
async fn should_give_the_system_agent_the_app_tools_the_host_registers_on_its_session() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let (ws, mut rx) = ws_connection_for_test(32);
    let tools = json!({ "tools": [calendar_today()] });

    // The host credential: a host token of an app peer this session prepared.
    let refused = register_on_session(&fx, &ws, None, &fx.system, tools.clone()).unwrap_err();
    assert_eq!(refused.data.unwrap()["kind"], "peer_host_token_mismatch");
    let other = SessionKey::with_profile_topic("dev", "api", &host_chat(), "other");
    let refused = register_on_session(&fx, &ws, Some(&token), &other, tools.clone()).unwrap_err();
    assert_eq!(refused.data.unwrap()["kind"], "peer_host_token_mismatch");
    let refused =
        register_on_session(&fx, &ws, Some(&token), &peer_key(&fx), tools.clone()).unwrap_err();
    assert_eq!(refused.data.unwrap()["kind"], "peer_tools_invalid");
    let (ext_ws, _ext_rx) = external_ws(8);
    assert!(register_on_session(&fx, &ext_ws, Some(&token), &fx.system, tools.clone()).is_err());

    let registered = register_on_session(&fx, &ws, Some(&token), &fx.system, tools).unwrap();
    assert_eq!(registered["version"], 1);
    assert_eq!(registered["tools"][0]["app"], "calendar");

    // The host's own turn on the system session: its usual tools plus the
    // granted app tool, routed to the host.
    let runtime = crate::runtime::SessionRuntime::bootstrap(&fx.runtime, fx.system.clone(), None)
        .await
        .unwrap();
    let mut host_turn = runtime.tools.snapshot_excluding(&[]);
    let usual = sorted_names(&host_turn);
    crate::peers::host_tools::apply_session_owned_host_tools(
        &mut host_turn,
        &peers_root(&fx),
        &fx.system,
        "turn-s1",
        Some(ws.connection_id.0),
    );
    assert_eq!(
        host_turn.origin("calendar_today"),
        Some(octos_agent::ToolOrigin::HostRouted)
    );
    assert!(usual.iter().all(|name| host_turn.get(name).is_some()));
    // Any other connection's turn (a web client, a continuation) is untouched.
    let (other_ws, _other_rx) = ws_connection_for_test(8);
    for connection in [Some(other_ws.connection_id.0), None] {
        let mut foreign = runtime.tools.snapshot_excluding(&[]);
        crate::peers::host_tools::apply_session_owned_host_tools(
            &mut foreign,
            &peers_root(&fx),
            &fx.system,
            "turn-s2",
            connection,
        );
        assert_eq!(sorted_names(&foreign), usual);
    }

    // The host's kernel tool list for the session narrows every turn on it:
    // the host's, other clients', kernel wake-ups; another session of the
    // profile is unaffected.
    let (list_ws, _list_rx) = ws_connection_for_test(8);
    register_on_session(
        &fx,
        &list_ws,
        Some(&token),
        &fx.system,
        json!({ "tools": [calendar_today()], "generic_tools": ["read_file", "ask_user_question"] }),
    )
    .unwrap();
    for connection in [
        Some(list_ws.connection_id.0),
        Some(other_ws.connection_id.0),
        None,
    ] {
        let mut narrowed = runtime.tools.snapshot_excluding(&[]);
        crate::peers::host_tools::apply_session_owned_host_tools(
            &mut narrowed,
            &peers_root(&fx),
            &fx.system,
            "turn-n",
            connection,
        );
        let names = sorted_names(&narrowed);
        let expected: Vec<&str> = if connection == Some(list_ws.connection_id.0) {
            vec!["ask_user_question", "calendar_today", "read_file"]
        } else {
            vec!["ask_user_question", "read_file"]
        };
        assert_eq!(names, expected, "{connection:?}");
    }
    let chat = SessionKey::with_profile_topic("dev", "api", &host_chat(), "chat");
    let mut elsewhere = runtime.tools.snapshot_excluding(&[]);
    crate::peers::host_tools::apply_session_owned_host_tools(
        &mut elsewhere,
        &peers_root(&fx),
        &chat,
        "turn-e",
        Some(list_ws.connection_id.0),
    );
    assert_eq!(sorted_names(&elsewhere), usual);
    crate::peers::host_tools::drop_routes_for_connection(list_ws.connection_id.0);
    // Registered again by the first host connection (as it was).
    register_on_session(
        &fx,
        &ws,
        Some(&token),
        &fx.system,
        json!({ "tools": [calendar_today()] }),
    )
    .unwrap();

    // A call reaches the host with the caller marked as the system agent.
    let state = fx.state.clone();
    let system = fx.system.clone();
    let host_token = token.clone();
    let connection = ws.connection_id.0;
    let host = tokio::spawn(async move {
        let call = next_frame(&mut rx, "peer/tool/call").await;
        raw_peer_tool_result(
            connection,
            &state,
            &rpc(
                APPUI_METHOD_PEER_TOOL_RESULT,
                json!({"session_id": system, "host_token": host_token,
                       "call_id": call["call_id"], "ok": true, "data": {"events": 2}}),
            ),
            None,
        )
        .expect("the host answers a session call");
        // A second answer to the same call is refused: once only.
        let again = raw_peer_tool_result(
            connection,
            &state,
            &rpc(
                APPUI_METHOD_PEER_TOOL_RESULT,
                json!({"session_id": system, "host_token": host_token,
                       "call_id": call["call_id"], "ok": true, "data": {}}),
            ),
            None,
        )
        .unwrap_err();
        assert_eq!(again.data.unwrap()["kind"], "peer_tool_call_not_found");
        call
    });
    let result = host_turn
        .execute_with_context(&call_ctx("c1"), "calendar_today", &json!({}))
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);
    let call = host.await.unwrap();
    assert_eq!(call["app"], "calendar");
    assert_eq!(call["peer"], Value::Null);
    assert_eq!(call["caller"]["kind"], "system");
    assert_eq!(call["caller"]["session_id"], json!(fx.system));
    let audit = std::fs::read_to_string(fx.data_dir.join("host_session_tool_audit.jsonl")).unwrap();
    assert!(audit.contains("\"app\":\"calendar\""), "{audit}");

    // The set lives as long as the host connection.
    crate::peers::host_tools::drop_routes_for_connection(ws.connection_id.0);
    let mut after = runtime.tools.snapshot_excluding(&[]);
    crate::peers::host_tools::apply_session_owned_host_tools(
        &mut after,
        &peers_root(&fx),
        &fx.system,
        "turn-s3",
        Some(ws.connection_id.0),
    );
    assert!(after.get("calendar_today").is_none());
}

/// OctoSense#146: the host's own connection (the private `serve --stdio`
/// pipe, or the host token's connection of `serve --host-managed`) registers
/// a host session's tools with no app peer's token, so the system agent's
/// host tools do not wait for an app peer to exist; it answers the calls
/// routed to it the same way. Any other connection still needs the token of
/// an app peer that session prepared, and an app peer's session is still
/// refused.
#[tokio::test]
async fn should_register_a_host_sessions_tools_without_a_peer_token_when_the_connection_is_the_hosts_own()
 {
    let tools = json!({ "tools": [calendar_today()] });

    // `serve --stdio`: the host's private pipe. No app peer exists yet.
    let fx = fixture().await;
    let (stdio_tx, _stdio_rx) = std::sync::mpsc::sync_channel(8);
    let stdio = WsConnection::new_stdio(stdio_tx);
    let registered = register_on_session(&fx, &stdio, None, &fx.system, tools.clone())
        .expect("the host's own pipe needs no app peer's token");
    assert_eq!(registered["version"], 1);
    let runtime = crate::runtime::SessionRuntime::bootstrap(&fx.runtime, fx.system.clone(), None)
        .await
        .unwrap();
    let mut host_turn = runtime.tools.snapshot_excluding(&[]);
    crate::peers::host_tools::apply_session_owned_host_tools(
        &mut host_turn,
        &peers_root(&fx),
        &fx.system,
        "turn-h1",
        Some(stdio.connection_id.0),
    );
    assert_eq!(
        host_turn.origin("calendar_today"),
        Some(octos_agent::ToolOrigin::HostRouted)
    );
    // Its answer to a call needs no token either (only the connection the
    // call was sent to may answer it).
    let answer = raw_peer_tool_result(
        stdio.connection_id.0,
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_TOOL_RESULT,
            json!({"session_id": fx.system, "call_id": "no-such-call", "ok": true, "data": {}}),
        ),
        None,
    )
    .unwrap_err();
    assert_eq!(answer.data.unwrap()["kind"], "peer_tool_call_not_found");
    // Still refused: an app peer's session, and another connection without
    // a token (or its answer).
    let refused = register_on_session(&fx, &stdio, None, &peer_key(&fx), tools.clone()).unwrap_err();
    assert_eq!(refused.data.unwrap()["kind"], "peer_tools_invalid");
    let (ws, _rx) = ws_connection_for_test(8);
    let refused = register_on_session(&fx, &ws, None, &fx.system, tools.clone()).unwrap_err();
    assert_eq!(refused.data.unwrap()["kind"], "peer_host_token_mismatch");
    let refused = raw_peer_tool_result(
        ws.connection_id.0,
        &fx.state,
        &rpc(
            APPUI_METHOD_PEER_TOOL_RESULT,
            json!({"session_id": fx.system, "call_id": "no-such-call", "ok": true, "data": {}}),
        ),
        None,
    )
    .unwrap_err();
    assert_eq!(refused.data.unwrap()["kind"], "peer_host_token_mismatch");
    crate::peers::host_tools::drop_routes_for_connection(stdio.connection_id.0);

    // `serve --host-managed`: the host token's connection needs none; an
    // external client is refused whatever it holds.
    let fx = fixture_with(|state| {
        state.host_managed = Some(Arc::new(
            super::super::host_managed::HostManaged::new(
                "host-token-0123456789abcdef0123456789abcdef".to_owned(),
                None,
                8765,
            )
            .unwrap(),
        ));
    })
    .await;
    let (host_ws, _host_rx) = ws_connection_for_test(8);
    register_on_session(&fx, &host_ws, None, &fx.system, tools.clone())
        .expect("the host token's connection needs no app peer's token");
    let (ext_ws, _ext_rx) = external_ws(8);
    assert!(register_on_session(&fx, &ext_ws, None, &fx.system, tools).is_err());
    crate::peers::host_tools::drop_routes_for_connection(host_ws.connection_id.0);
}

#[tokio::test]
async fn should_offer_ask_user_question_on_a_peer_input_turn_when_the_host_lists_it() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, mut rx) = ws_connection_for_test(32);
    register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [news_list()], "generic_tools": ["ask_user_question", "read_file"] }),
    )
    .unwrap();
    deliver_peer_send_input(
        "dev",
        &peers_root(&fx),
        &fx.system.0,
        &TurnId::new(),
        send_input_request("ask me something", "call_1"),
    )
    .unwrap();
    let input = next_frame(&mut rx, "peer/input").await;
    let registry = turn_registry(&fx, &key, input["turn_id"].as_str().unwrap()).await;
    assert_eq!(
        sorted_names(&registry),
        ["ask_user_question", "news_list", "read_file"]
    );
}

// ---------------------------------------------------------------------------
// #2567 post-approval follow-ups (N1–N4)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_never_send_a_call_of_a_turn_that_was_interrupted() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, mut rx) = ws_connection_for_test(32);
    register(
        &fx,
        &ws,
        &token,
        json!({ "tools": [news_list()], "call_timeout_ms": 300 }),
    )
    .unwrap();
    let registry = turn_registry(&fx, &key, "turn-int").await;
    // The person interrupted the turn before this call reached the host (its
    // tool task was still in a hook, or its approval had just been answered).
    crate::peers::host_tools::cancel_host_calls_for_turn(&key, "turn-int");
    let result = registry
        .execute_with_context(&call_ctx("c1"), "news_list", &json!({}))
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.output.contains("interrupted"), "{}", result.output);
    assert!(rx.try_recv().is_err(), "no peer/tool/call was sent");
    // Another turn of the same session is unaffected.
    let host = spawn_fake_host(
        &fx,
        token.clone(),
        rx,
        |_| json!({ "ok": true, "data": [] }),
    );
    let later = turn_registry(&fx, &key, "turn-next").await;
    assert!(
        later
            .execute_with_context(&call_ctx("c2"), "news_list", &json!({}))
            .await
            .unwrap()
            .success
    );
    drop(ws);
    crate::peers::host_tools::set_host_route(&peers_root(&fx), "news", 0, Arc::new(|_, _| false));
    assert_eq!(host.await.unwrap().len(), 1);
}

#[tokio::test]
async fn should_refuse_foreign_monitors_and_deletes_on_a_host_peer_session() {
    use crate::autonomy::agent_orchestrator::{
        AgentOrchestrator, MonitorControlKind, MonitorControlRequest, MonitorCreateRequest,
    };
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (host_ws, _host_rx) = ws_connection_for_test(8);
    let (other_ws, _other_rx) = ws_connection_for_test(8);
    register(&fx, &host_ws, &token, json!({ "tools": [] })).unwrap();
    for (method, params) in [
        (
            "monitor/create",
            json!({ "session_id": key, "name": "m", "argv": ["true"] }),
        ),
        ("session/delete", json!({ "session_id": key })),
        // An unknown monitor id falls back to the named session.
        (
            "monitor/resume",
            json!({ "monitor_id": "mon-unknown", "session_id": key }),
        ),
    ] {
        let error = refuse_foreign_host_peer_session_call(&fx.state, &other_ws, method, &params)
            .unwrap_or_else(|| panic!("{method}"));
        assert_eq!(error.data.unwrap()["kind"], "peer_host_connection_only");
        assert!(
            refuse_foreign_host_peer_session_call(&fx.state, &host_ws, method, &params).is_none()
        );
    }
    // A monitor of the peer's session, resumed by naming the host's base
    // session (a base key controls the monitors of its topics): its target
    // is the monitor's session, so it is refused too.
    let orchestrator = crate::autonomy::agent_orchestrator::default_agent_orchestrator();
    let created = orchestrator
        .create_monitor(MonitorCreateRequest {
            session_id: key.clone(),
            profile_id: "dev".into(),
            spec: crate::autonomy::monitor_runtime::MonitorSpec {
                name: "peer-monitor".into(),
                argv: vec!["sh".into(), "-c".into(), "true".into()],
                filter_regex: None,
                batch_ms: crate::autonomy::monitor_runtime::MONITOR_DEFAULT_BATCH_MS,
                mode: crate::autonomy::monitor_runtime::MonitorMode::Poll {
                    interval_secs: 3_600,
                },
                timeout_secs: None,
                persistent: false,
                max_events_per_hour: 1,
                goal_id: None,
                cwd: None,
            },
            data_dir: None,
        })
        .expect("create monitor");
    let monitor_id = created["monitor_id"].as_str().unwrap().to_owned();
    let resume = json!({ "monitor_id": monitor_id, "session_id": fx.system });
    assert!(
        refuse_foreign_host_peer_session_call(&fx.state, &other_ws, "monitor/resume", &resume)
            .is_some()
    );
    assert!(
        refuse_foreign_host_peer_session_call(&fx.state, &host_ws, "monitor/resume", &resume)
            .is_none()
    );
    let _ = orchestrator.control_monitor(MonitorControlRequest {
        monitor_id,
        session_id: Some(key),
        profile_id: "dev".into(),
        kind: MonitorControlKind::Delete,
    });
}

#[tokio::test]
async fn should_refuse_a_foreign_stdio_turn_control_from_the_persisted_set() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    // The host registered on its own connection; nothing is in memory about
    // the peer's session yet (as after a restart: no turn has run).
    let (host_ws, _host_rx) = ws_connection_for_test(8);
    register(&fx, &host_ws, &token, json!({ "tools": [] })).unwrap();
    let (mut input_tx, input_rx) = tokio::io::duplex(8192);
    let (mut output_rx, output_tx) = tokio::io::duplex(65536);
    for (id, method, params) in [
        (
            "s1",
            "turn/steer",
            json!({ "session_id": key, "expected_turn_id": TurnId::new(), "input": [{"kind": "text", "text": "x"}] }),
        ),
        (
            "s2",
            "turn/interrupt",
            json!({ "session_id": key, "turn_id": TurnId::new() }),
        ),
    ] {
        let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        input_tx
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
    }
    drop(input_tx);
    stdio_connection_with_io(
        fx.state.clone(),
        input_rx,
        output_tx,
        new_stdio_dispatch_count_for_test(),
        None,
    )
    .await
    .expect("connection exits on EOF");
    let mut out = String::new();
    output_rx.read_to_string(&mut out).await.unwrap();
    for id in ["s1", "s2"] {
        let reply: Value = out
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|frame| frame["id"] == id)
            .unwrap_or_else(|| panic!("a reply to {id}: {out}"));
        assert_eq!(
            reply["error"]["data"]["kind"], "peer_host_connection_only",
            "{id}: {reply}"
        );
    }
}

#[tokio::test]
async fn should_end_in_flight_calls_when_a_send_to_the_host_fails() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, mut rx) = ws_connection_for_test(32);
    register(&fx, &ws, &token, json!({ "tools": [news_topics_set()] })).unwrap();
    let registry = Arc::new(turn_registry(&fx, &key, "turn-1").await);
    let task = {
        let registry = registry.clone();
        tokio::spawn(async move {
            registry
                .execute_with_context(
                    &call_ctx("c1"),
                    "news_topics_set",
                    &json!({"topics": ["rust"]}),
                )
                .await
                .unwrap()
        })
    };
    next_frame(&mut rx, "peer/tool/call").await;
    // The host's socket is gone, but no close was observed yet: the next
    // send to it (here a `peer/input`) fails and drops the route.
    drop(rx);
    assert!(
        deliver_peer_send_input(
            "dev",
            &peers_root(&fx),
            &fx.system.0,
            &TurnId::new(),
            send_input_request("hello", "call_9"),
        )
        .is_err()
    );
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("the in-flight call ends at once, not after its timeout")
        .unwrap();
    assert!(!result.success);
    assert!(
        result.output.contains("outcome_unknown"),
        "{}",
        result.output
    );
}

// ---------------------------------------------------------------------------
// #2618 — `peer/input/reject`: the host refuses a `peer/input`
// ---------------------------------------------------------------------------

/// `peer/input/reject` from `connection`.
fn reject_input(
    fx: &Fx,
    connection: u64,
    token: &str,
    input_id: &str,
    extra: Value,
) -> Result<Value, RpcError> {
    let mut params = json!({
        "session_id": fx.system,
        "peer": "news",
        "host_token": token,
        "input_id": input_id,
    });
    for (key, value) in extra.as_object().unwrap() {
        params[key] = value.clone();
    }
    raw_peer_input_reject(
        connection,
        &fx.state,
        &rpc(APPUI_METHOD_PEER_INPUT_REJECT, params),
        None,
    )
}

/// The system agent's `peer_send_input`, wired as a system turn wires it,
/// waiting `wait` for the host's answer.
fn system_send_input_tool(fx: &Fx, wait: std::time::Duration) -> octos_agent::PeerSendInputTool {
    let peers = peers_root(fx);
    let system = fx.system.0.clone();
    let turn = TurnId::new();
    let answer_turn = turn.clone();
    let send: octos_agent::PeerSendInputCallback =
        Arc::new(move |req| deliver_peer_send_input("dev", &peers, &system, &turn, req));
    octos_agent::PeerSendInputTool::new(send).with_answer(peer_send_input_answer_callback(
        fx.system.0.clone(),
        answer_turn,
        wait,
    ))
}

/// Deliver one input to the host on `rx`; returns the `peer/input` params.
async fn deliver_input(fx: &Fx, rx: &mut mpsc::Receiver<WsMessage>, occurrence: &str) -> Value {
    deliver_peer_send_input(
        "dev",
        &peers_root(fx),
        &fx.system.0,
        &TurnId::new(),
        send_input_request("summarise today's news", occurrence),
    )
    .expect("delivered");
    next_frame(rx, "peer/input").await
}

fn rpc_kind(error: RpcError) -> String {
    error.data.expect("typed error")["kind"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn should_fail_the_waiting_peer_send_input_with_the_hosts_reason() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let (ws, mut rx) = ws_connection_for_test(16);
    register(&fx, &ws, &token, json!({ "tools": [] })).unwrap();
    let connection = ws.connection_id.0;
    // The host refuses the input as soon as it arrives.
    let host = {
        let state = fx.state.clone();
        let system = fx.system.clone();
        let token = token.clone();
        tokio::spawn(async move {
            let input = next_frame(&mut rx, "peer/input").await;
            raw_peer_input_reject(
                connection,
                &state,
                &rpc(
                    APPUI_METHOD_PEER_INPUT_REJECT,
                    json!({"session_id": system, "peer": "news", "host_token": token,
                           "input_id": input["input_id"], "reason": "no_consent"}),
                ),
                None,
            )
        })
    };

    let tool = system_send_input_tool(&fx, std::time::Duration::from_secs(10));
    let started = std::time::Instant::now();
    let result = octos_agent::Tool::execute_with_context(
        &tool,
        &call_ctx("call_1"),
        &json!({"slug": "news", "message": "summarise today's news"}),
    )
    .await
    .unwrap();
    assert!(!result.success, "{}", result.output);
    assert!(
        result.output.contains("peer_input_rejected: no_consent"),
        "{}",
        result.output
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "the refusal ends the wait"
    );
    let accepted = host.await.unwrap().expect("the refusal is accepted");
    assert_eq!(accepted["rejected"], true);
    assert_eq!(accepted["reported_to"], "call");
    // Told through the call: nothing is left for the system session's next turn.
    assert!(
        crate::peers::host_tools::peer_input_rejections_note(&peers_root(&fx), &fx.system)
            .is_none()
    );
    let decisions: Vec<Value> = audit_rows(&fx)
        .into_iter()
        .filter_map(|row| row.get("decision").cloned())
        .collect();
    assert_eq!(
        decisions,
        vec![json!("peer_input_sent"), json!("peer_input_rejected")]
    );
    let rejected = audit_rows(&fx).pop().unwrap();
    assert_eq!(rejected["outcome"], "no_consent");
    assert_eq!(rejected["reported_to"], "call");
}

#[tokio::test]
async fn should_end_the_wait_without_an_error_when_the_host_starts_the_turn() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let (ws, mut rx) = ws_connection_for_test(16);
    register(&fx, &ws, &token, json!({ "tools": [] })).unwrap();
    let host = {
        let state = fx.state.clone();
        let peer = peer_key(&fx);
        tokio::spawn(async move {
            let input = next_frame(&mut rx, "peer/input").await;
            let turn: TurnId = serde_json::from_value(input["turn_id"].clone()).unwrap();
            claim_peer_input_turn(&state, &peer, &turn)
        })
    };
    let tool = system_send_input_tool(&fx, std::time::Duration::from_secs(10));
    let started = std::time::Instant::now();
    let result = octos_agent::Tool::execute_with_context(
        &tool,
        &call_ctx("call_1"),
        &json!({"slug": "news", "message": "hello"}),
    )
    .await
    .unwrap();
    assert!(result.success, "{}", result.output);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    host.await.unwrap().expect("the turn start is accepted");
}

#[tokio::test]
async fn should_report_a_rejection_after_the_call_returned_on_the_system_session() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let (ws, mut rx) = ws_connection_for_test(16);
    register(&fx, &ws, &token, json!({ "tools": [] })).unwrap();
    // The host does not answer within the call's wait: the call reports the
    // input as sent.
    let tool = system_send_input_tool(&fx, std::time::Duration::from_millis(50));
    let result = octos_agent::Tool::execute_with_context(
        &tool,
        &call_ctx("call_1"),
        &json!({"slug": "news", "message": "summarise today's news"}),
    )
    .await
    .unwrap();
    assert!(result.success, "{}", result.output);
    let input = next_frame(&mut rx, "peer/input").await;

    let accepted = reject_input(
        &fx,
        ws.connection_id.0,
        &token,
        input["input_id"].as_str().unwrap(),
        json!({"reason": "other", "message": "the News account is suspended"}),
    )
    .expect("accepted after the call returned");
    assert_eq!(accepted["reported_to"], "system_session");
    // On the peer's blackboard...
    let log = std::fs::read_to_string(peers_root(&fx).join("news/input_rejections.jsonl")).unwrap();
    assert!(log.contains("\"reason\":\"other\""), "{log}");
    // ...and told to the system session at its next turn, once.
    let note = crate::peers::host_tools::peer_input_rejections_note(&peers_root(&fx), &fx.system)
        .expect("a note for the system session");
    assert!(
        note.contains("News (news): peer_input_rejected: other (the News account is suspended)")
            || note.contains("news: peer_input_rejected: other (the News account is suspended)"),
        "{note}"
    );
    assert!(
        crate::peers::host_tools::peer_input_rejections_note(&peers_root(&fx), &fx.system)
            .is_none(),
        "reported once"
    );
    // Never to the peer's own session or another session.
    assert!(
        crate::peers::host_tools::peer_input_rejections_note(&peers_root(&fx), &peer_key(&fx))
            .is_none()
    );
    let rejected = audit_rows(&fx).pop().unwrap();
    assert_eq!(rejected["decision"], "peer_input_rejected");
    assert_eq!(rejected["reported_to"], "system_session");
}

#[tokio::test]
async fn should_refuse_a_rejection_from_a_foreign_connection_with_a_bad_token_twice_or_after_the_turn_started()
 {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let (ws, mut rx) = ws_connection_for_test(16);
    register(&fx, &ws, &token, json!({ "tools": [] })).unwrap();
    let host = ws.connection_id.0;
    let (other, _other_rx) = ws_connection_for_test(16);
    let busy = json!({"reason": "busy"});

    let input = deliver_input(&fx, &mut rx, "call_1").await;
    let input_id = input["input_id"].as_str().unwrap();
    // Another connection of the profile, even holding the token.
    assert_eq!(
        rpc_kind(
            reject_input(&fx, other.connection_id.0, &token, input_id, busy.clone()).unwrap_err()
        ),
        "peer_input_wrong_connection"
    );
    // The host connection without the token.
    assert_eq!(
        rpc_kind(reject_input(&fx, host, "guess", input_id, busy.clone()).unwrap_err()),
        "peer_host_token_mismatch"
    );
    // An input the kernel never sent.
    assert_eq!(
        rpc_kind(reject_input(&fx, host, &token, "no-such-input", busy.clone()).unwrap_err()),
        "peer_input_not_found"
    );
    // Accepted once...
    reject_input(&fx, host, &token, input_id, busy.clone()).expect("the host refuses it");
    // ...never twice.
    assert_eq!(
        rpc_kind(reject_input(&fx, host, &token, input_id, busy.clone()).unwrap_err()),
        "peer_input_already_rejected"
    );

    // An input whose turn the host already started is answered.
    let input = deliver_input(&fx, &mut rx, "call_2").await;
    let turn: TurnId = serde_json::from_value(input["turn_id"].clone()).unwrap();
    claim_peer_input_turn(&fx.state, &peer_key(&fx), &turn).expect("the host starts the turn");
    assert_eq!(
        rpc_kind(
            reject_input(&fx, host, &token, input["input_id"].as_str().unwrap(), busy).unwrap_err()
        ),
        "peer_input_already_started"
    );
}

#[tokio::test]
async fn should_refuse_an_unknown_reason_an_unknown_field_or_a_bad_message() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let (ws, mut rx) = ws_connection_for_test(16);
    register(&fx, &ws, &token, json!({ "tools": [] })).unwrap();
    let host = ws.connection_id.0;
    let input = deliver_input(&fx, &mut rx, "call_1").await;
    let input_id = input["input_id"].as_str().unwrap();

    for (extra, kind) in [
        (
            json!({"reason": "offline"}),
            Some("peer_input_reject_invalid"),
        ),
        (json!({"reason": "Busy"}), Some("peer_input_reject_invalid")),
        (
            json!({"reason": "other", "message": "x".repeat(257)}),
            Some("peer_input_reject_invalid"),
        ),
        (
            json!({"reason": "other"}),
            Some("peer_input_reject_invalid"),
        ),
        (
            json!({"reason": "other", "message": "  "}),
            Some("peer_input_reject_invalid"),
        ),
        (
            json!({"reason": "other", "message": "line one\nline two"}),
            Some("peer_input_reject_invalid"),
        ),
        (
            json!({"reason": "busy", "message": "queue full"}),
            Some("peer_input_reject_invalid"),
        ),
        // Unknown fields are refused by the parser (no typed kind).
        (json!({"reason": "busy", "retry_after": 30}), None),
    ] {
        let error = reject_input(&fx, host, &token, input_id, extra.clone())
            .expect_err(&format!("{extra} is refused"));
        if let Some(kind) = kind {
            assert_eq!(rpc_kind(error), kind, "{extra}");
        }
    }
    // None of those consumed the input: a well-formed refusal is accepted.
    let accepted = reject_input(
        &fx,
        host,
        &token,
        input_id,
        json!({"reason": "other", "message": "x".repeat(256)}),
    )
    .expect("a 256-byte message is accepted");
    assert_eq!(accepted["rejected"], true);
}

#[tokio::test]
async fn should_accept_a_busy_rejection_when_the_hosts_turn_start_was_refused() {
    // Security review (ADR 0004): a `turn/start` with an input's turn id that
    // the kernel then refuses (here: no text input) must not consume the
    // input — the host can still answer it with `peer/input/reject`.
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let (ws, mut rx) = ws_connection_for_test(64);
    register(&fx, &ws, &token, json!({ "tools": [] })).unwrap();
    let input = deliver_input(&fx, &mut rx, "call_1").await;
    let turn: TurnId = serde_json::from_value(input["turn_id"].clone()).unwrap();

    let ledger = Arc::new(UiProtocolLedger::new(16));
    let contracts = Arc::new(UiProtocolContractStores::default());
    let active_turns: SharedActiveTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let connection_turns: SharedConnectionTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let started = handle_turn_start(
        &ws,
        &fx.state,
        &ledger,
        &contracts,
        &active_turns,
        &connection_turns,
        None,
        None,
        ConnectionUiFeatures::stdio_defaults(),
        "start-refused".into(),
        TurnStartParams {
            session_id: peer_key(&fx),
            turn_id: turn.clone(),
            input: Vec::new(),
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
    assert!(!started, "a turn/start without text input is refused");
    assert!(active_turns.lock().await.is_empty());

    let accepted = reject_input(
        &fx,
        ws.connection_id.0,
        &token,
        input["input_id"].as_str().unwrap(),
        json!({"reason": "busy"}),
    )
    .expect("the refused start did not answer the input");
    assert_eq!(accepted["rejected"], true);
}

#[tokio::test]
async fn should_refuse_a_turn_start_with_a_rejected_inputs_turn_id() {
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let (ws, mut rx) = ws_connection_for_test(64);
    register(&fx, &ws, &token, json!({ "tools": [] })).unwrap();
    let input = deliver_input(&fx, &mut rx, "call_1").await;
    let turn: TurnId = serde_json::from_value(input["turn_id"].clone()).unwrap();
    reject_input(
        &fx,
        ws.connection_id.0,
        &token,
        input["input_id"].as_str().unwrap(),
        json!({"reason": "signed_out"}),
    )
    .unwrap();

    let ledger = Arc::new(UiProtocolLedger::new(16));
    let contracts = Arc::new(UiProtocolContractStores::default());
    let active_turns: SharedActiveTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let connection_turns: SharedConnectionTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let started = handle_turn_start(
        &ws,
        &fx.state,
        &ledger,
        &contracts,
        &active_turns,
        &connection_turns,
        None,
        None,
        ConnectionUiFeatures::stdio_defaults(),
        "start-rejected".into(),
        TurnStartParams {
            session_id: peer_key(&fx),
            turn_id: turn.clone(),
            input: vec![InputItem::Text {
                text: input["text"].as_str().unwrap().into(),
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
    assert!(!started, "the turn is refused");
    let refused = loop {
        let frame = frame_json(
            tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("a frame in time")
                .expect("connection open"),
        );
        if frame["id"] == "start-rejected" {
            break frame;
        }
    };
    assert_eq!(
        refused["error"]["data"]["kind"], "peer_input_rejected",
        "{refused}"
    );
    assert!(active_turns.lock().await.is_empty());
    // The turn id is released: it no longer counts as the system agent's
    // request (a foreground app tool would not run in it).
    let mut foreground = news_list();
    foreground["background"] = json!(false);
    register(&fx, &ws, &token, json!({ "tools": [foreground] })).unwrap();
    let registry = turn_registry(&fx, &peer_key(&fx), &turn.0.to_string()).await;
    let approver = app_approver(
        &fx,
        &peer_key(&fx),
        &Arc::new(UiProtocolContractStores::default()),
        &TurnId::new(),
    );
    let refused = octos_agent::tools::TOOL_APPROVAL_CTX
        .scope(
            approver,
            registry.execute_with_context(&call_ctx("c1"), "news_list", &json!({})),
        )
        .await
        .unwrap();
    assert!(
        !refused.success && refused.output.contains("background"),
        "{}",
        refused.output
    );
}

// ---------------------------------------------------------------------------
// The shared peer conversation: the person and the system agent both drive
// the host-owned peer's own session, and each turn says who is speaking.
// ---------------------------------------------------------------------------

fn origin(
    kind: octos_core::ui_protocol::TurnOriginKind,
    label: Option<&str>,
) -> octos_core::ui_protocol::TurnOrigin {
    octos_core::ui_protocol::TurnOrigin {
        kind,
        label: label.map(str::to_owned),
    }
}

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

/// The first frame matching `pick`, waiting up to 10 s.
async fn wait_frame(frames: &Frames, pick: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..500 {
        if let Some(frame) = frames.lock().unwrap().iter().find(|f| pick(f)) {
            return frame.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("no matching frame in {:?}", frames.lock().unwrap());
}

/// `data.kind` of the error reply to request `id`.
async fn error_kind_of(frames: &Frames, id: &str) -> Value {
    let reply = wait_frame(frames, |f| f["id"] == id).await;
    reply["error"]["data"]["kind"].clone()
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
    for _ in 0..1500 {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_run_a_persons_turn_on_the_peer_session_labelled_with_its_origin() {
    use octos_core::ui_protocol::TurnOriginKind;
    let llm = Arc::new(RecordingLlm::default());
    let mut e = e2e_fixture(llm.clone(), json!({ "tools": [] })).await;
    let _frames = collect_frames(e.rx.take().unwrap());
    let peer = shared_peer(&e);
    let active: SharedActiveTurns = Arc::new(TokioMutex::new(HashMap::new()));

    // The person, through the host, on the peer's own session.
    let person_turn = TurnId::new();
    assert!(
        start_turn(
            &e,
            &e.ws,
            &active,
            "p1",
            &peer,
            &person_turn,
            "what's new?",
            Some(origin(TurnOriginKind::Person, Some("Ada"))),
        )
        .await
    );
    let result = wait_result(&e, &person_turn).await;
    assert!(result.contains("\norigin: person\n"), "{result}");
    let first = llm.requests.lock().unwrap()[0].clone();
    assert!(
        first.contains("[from the person: Ada] what's new?"),
        "{first}"
    );
    // The person's round alone never fires a synthesis on the system agent:
    // it is recorded as covered.
    let peers = e.data_dir.join("peers");
    let mut covered = false;
    for _ in 0..250 {
        if matches!(
            read_peer_fleet_synthesis_marks(&peers, &e.system.0),
            FleetSynthesisMarks::Rounds(ref rounds) if rounds.get("news") == Some(&1)
        ) {
            covered = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(covered, "the person's round is recorded as covered");
    assert_eq!(
        crate::autonomy::agent_orchestrator::default_agent_orchestrator()
            .pending_continuation_count_for_session_for_test(&e.system, "dev"),
        0,
        "no synthesis turn queued on the system agent"
    );

    // The next turn (the app's) sees the person's labelled row in the shared
    // history, and its own label.
    // (`result.md` lands just before the terminal: retry while the person's
    // turn is still finishing.)
    let app_turn = TurnId::new();
    let mut admitted = false;
    for attempt in 0..250 {
        if start_turn(
            &e,
            &e.ws,
            &active,
            &format!("p2-{attempt}"),
            &peer,
            &app_turn,
            "refresh the feed",
            Some(origin(TurnOriginKind::App, None)),
        )
        .await
        {
            admitted = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(admitted, "the app's turn starts once the person's ended");
    let result = wait_result(&e, &app_turn).await;
    assert!(result.contains("\norigin: app\n"), "{result}");
    let second = llm.requests.lock().unwrap()[1].clone();
    assert!(
        second.contains("[from the person: Ada] what's new?"),
        "{second}"
    );
    assert!(
        second.contains("[from the app] refresh the feed"),
        "{second}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_label_a_peer_input_turn_as_the_system_agents_and_refuse_a_relabel() {
    use octos_core::ui_protocol::TurnOriginKind;
    let llm = Arc::new(RecordingLlm::default());
    let mut e = e2e_fixture(llm.clone(), json!({ "tools": [] })).await;
    let frames = collect_frames(e.rx.take().unwrap());
    let peer = shared_peer(&e);
    let active: SharedActiveTurns = Arc::new(TokioMutex::new(HashMap::new()));
    deliver_peer_send_input(
        "dev",
        &e.data_dir.join("peers"),
        &e.system.0,
        &TurnId::new(),
        send_input_request("SUMMARIZE_TODAY", "call_1"),
    )
    .expect("delivered to the host");
    let input = wait_frame(&frames, |f| f["method"] == "peer/input").await;
    let turn_id: TurnId = serde_json::from_value(input["params"]["turn_id"].clone()).unwrap();

    // The host cannot pass the system agent's input off as the person's...
    for (id, kind) in [("r1", TurnOriginKind::Person), ("r2", TurnOriginKind::App)] {
        assert!(
            !start_turn(
                &e,
                &e.ws,
                &active,
                id,
                &peer,
                &turn_id,
                "SUMMARIZE_TODAY",
                Some(origin(kind, None)),
            )
            .await
        );
        assert_eq!(error_kind_of(&frames, id).await, "turn_origin_mismatch");
    }
    // ...nor label its own turn as the system agent's.
    assert!(
        !start_turn(
            &e,
            &e.ws,
            &active,
            "r3",
            &peer,
            &TurnId::new(),
            "do it",
            Some(origin(TurnOriginKind::SystemAgent, None)),
        )
        .await
    );
    assert_eq!(error_kind_of(&frames, "r3").await, "turn_origin_mismatch");

    // Started as `peer/input` asks (no origin), the kernel labels it.
    assert!(
        start_turn(
            &e,
            &e.ws,
            &active,
            "r4",
            &peer,
            &turn_id,
            "SUMMARIZE_TODAY",
            None
        )
        .await
    );
    let result = wait_result(&e, &turn_id).await;
    assert!(result.contains("\norigin: system_agent\n"), "{result}");
    let request = llm.requests.lock().unwrap()[0].clone();
    assert!(
        request.contains("[from the system agent] SUMMARIZE_TODAY"),
        "{request}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_refuse_a_turn_origin_from_other_connections_and_on_other_sessions() {
    use octos_core::ui_protocol::TurnOriginKind;
    let llm = Arc::new(RecordingLlm::default());
    let mut e = e2e_fixture(llm.clone(), json!({ "tools": [] })).await;
    let host_frames = collect_frames(e.rx.take().unwrap());
    let peer = shared_peer(&e);
    let active: SharedActiveTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let person = || Some(origin(TurnOriginKind::Person, None));

    // Another connection of the profile, and an external client.
    let (foreign, foreign_rx) = ws_connection_for_test(64);
    let foreign_frames = collect_frames(foreign_rx);
    assert!(
        !start_turn(
            &e,
            &foreign,
            &active,
            "f1",
            &peer,
            &TurnId::new(),
            "hi",
            person()
        )
        .await
    );
    assert_eq!(
        error_kind_of(&foreign_frames, "f1").await,
        "turn_origin_host_only"
    );
    let (external, external_rx) = external_ws(64);
    let external_frames = collect_frames(external_rx);
    assert!(
        !start_turn(
            &e,
            &external,
            &active,
            "x1",
            &peer,
            &TurnId::new(),
            "hi",
            person()
        )
        .await
    );
    assert_eq!(
        error_kind_of(&external_frames, "x1").await,
        "turn_origin_host_only"
    );
    // The existing gates in front of the handler still refuse both outright.
    let params = json!({
        "session_id": peer,
        "turn_id": TurnId::new(),
        "input": [{"kind": "text", "text": "hi"}],
        "origin": {"kind": "person"},
    });
    let refused = refuse_foreign_host_peer_session_call(&e.state, &foreign, "turn/start", &params)
        .expect("a foreign turn/start on the host peer's session is refused");
    assert_eq!(refused.data.unwrap()["kind"], "peer_host_connection_only");
    // (The external identity is the `_main` profile: the same peer session
    // there.)
    let mut main_params = params.clone();
    main_params["session_id"] = json!(SessionKey::with_profile_topic(
        MAIN_PROFILE_ID,
        "api",
        &host_chat(),
        "peer-news"
    ));
    let refused = super::super::host_managed::external_gate(
        "turn/start",
        &main_params,
        &std::collections::HashSet::new(),
    )
    .expect_err("an external turn/start on a host peer's session is refused");
    assert_eq!(
        refused.data.unwrap()["kind"],
        super::super::host_managed::HOST_OWNED_PEER_SESSION_DENIED
    );

    // Only the peer's own session takes an origin: not the system agent's
    // session, and not a request context.
    let opened = raw_peer_context_open(
        &e.state,
        &rpc(
            APPUI_METHOD_PEER_CONTEXT_OPEN,
            json!({"session_id": e.system, "peer": "news", "context_id": "ui-1",
                   "host_token": e.token}),
        ),
        None,
    )
    .unwrap();
    let context: SessionKey = serde_json::from_value(opened["session_id"].clone()).unwrap();
    for (id, session) in [("o1", &e.system), ("o2", &context)] {
        assert!(
            !start_turn(
                &e,
                &e.ws,
                &active,
                id,
                session,
                &TurnId::new(),
                "hi",
                person()
            )
            .await
        );
        assert_eq!(
            error_kind_of(&host_frames, id).await,
            "turn_origin_not_allowed"
        );
    }
    assert!(
        llm.requests.lock().unwrap().is_empty(),
        "no refused turn ran"
    );
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
        Ok(octos_llm::ChatResponse {
            content: Some("ok".into()),
            reasoning_content: None,
            tool_calls: Vec::new(),
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: octos_llm::TokenUsage {
                input_tokens: 1,
                output_tokens: 1,
                ..Default::default()
            },
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "gated"
    }

    fn provider_name(&self) -> &str {
        "stub"
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_refuse_a_second_turn_while_the_shared_peer_session_is_busy() {
    use octos_core::ui_protocol::TurnOriginKind;
    let llm = Arc::new(GatedLlm::default());
    let mut e = e2e_fixture(llm.clone(), json!({ "tools": [] })).await;
    let frames = collect_frames(e.rx.take().unwrap());
    let peer = shared_peer(&e);
    let active: SharedActiveTurns = Arc::new(TokioMutex::new(HashMap::new()));

    let person_turn = TurnId::new();
    assert!(
        start_turn(
            &e,
            &e.ws,
            &active,
            "b1",
            &peer,
            &person_turn,
            "hello",
            Some(origin(TurnOriginKind::Person, None)),
        )
        .await
    );
    tokio::time::timeout(std::time::Duration::from_secs(20), llm.entered.notified())
        .await
        .expect("the person's turn reached the model");

    // The system agent's input arrives while the person's turn runs: the
    // kernel admits one turn per session, so its start is refused and the
    // host keeps it queued.
    deliver_peer_send_input(
        "dev",
        &e.data_dir.join("peers"),
        &e.system.0,
        &TurnId::new(),
        send_input_request("CHECK_MAIL", "call_1"),
    )
    .unwrap();
    let input = wait_frame(&frames, |f| f["method"] == "peer/input").await;
    let input_turn: TurnId = serde_json::from_value(input["params"]["turn_id"].clone()).unwrap();
    assert!(
        !start_turn(
            &e,
            &e.ws,
            &active,
            "b2",
            &peer,
            &input_turn,
            "CHECK_MAIL",
            None
        )
        .await
    );
    assert_eq!(error_kind_of(&frames, "b2").await, "turn_in_progress");
    // Another person's message is refused the same way.
    assert!(
        !start_turn(
            &e,
            &e.ws,
            &active,
            "b3",
            &peer,
            &TurnId::new(),
            "and another thing",
            Some(origin(TurnOriginKind::Person, None)),
        )
        .await
    );
    assert_eq!(error_kind_of(&frames, "b3").await, "turn_in_progress");

    // Once the person's turn ends, the host starts the queued input with the
    // same turn id, still labelled as the system agent's.
    llm.release.notify_one();
    let result = wait_result(&e, &person_turn).await;
    assert!(result.contains("\norigin: person\n"), "{result}");
    let mut admitted = false;
    for _ in 0..250 {
        if start_turn(
            &e,
            &e.ws,
            &active,
            "b4",
            &peer,
            &input_turn,
            "CHECK_MAIL",
            None,
        )
        .await
        {
            admitted = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(admitted, "the queued input starts once the session is free");
    let result = wait_result(&e, &input_turn).await;
    assert!(result.contains("\norigin: system_agent\n"), "{result}");
}

#[tokio::test]
async fn should_not_rearm_the_fleet_synthesis_for_a_persons_round_alone() {
    let fx = fixture().await;
    prepare_news(&fx).await;
    let peers = peers_root(&fx);
    let dir = peers.join("news");
    let owed = || {
        let marks = read_peer_fleet_synthesis_marks(&peers, &fx.system.0);
        let fleet = collect_owned_peer_results(&peers, &fx.system.0).unwrap();
        peer_fleet_synthesis_is_owed(&marks, &fleet)
    };
    let deliver = |round: u32| {
        std::fs::write(dir.join(format!("result-{round}.md")), "r").unwrap();
        std::fs::write(dir.join("result.md"), "r").unwrap();
    };

    // The person's first round: covered, nothing owed.
    deliver(1);
    absorb_person_round_into_fleet_marks(&peers, &dir, "news");
    assert!(!owed(), "a person's round alone owes no synthesis");
    // The system agent's round: owed as before.
    deliver(2);
    assert!(owed());
    // A person's round while that is still owed leaves it owed (the
    // synthesis then covers both).
    deliver(3);
    absorb_person_round_into_fleet_marks(&peers, &dir, "news");
    assert!(owed(), "the system agent's owed round still fires");
    assert!(matches!(
        read_peer_fleet_synthesis_marks(&peers, &fx.system.0),
        FleetSynthesisMarks::Rounds(ref rounds) if rounds.get("news") == Some(&1)
    ));
}

#[tokio::test]
async fn should_run_a_foreground_tool_in_the_persons_turn_on_the_peer_session() {
    use octos_core::ui_protocol::TurnOriginKind;
    let fx = fixture().await;
    let token = prepare_news(&fx).await;
    let key = peer_key(&fx);
    let (ws, rx) = ws_connection_for_test(32);
    let mut foreground = news_list();
    foreground["background"] = json!(false);
    register(&fx, &ws, &token, json!({ "tools": [foreground] })).unwrap();
    let host = spawn_fake_host(
        &fx,
        token.clone(),
        rx,
        |_| json!({ "ok": true, "data": [] }),
    );
    let approver = app_approver(
        &fx,
        &key,
        &Arc::new(UiProtocolContractStores::default()),
        &TurnId::new(),
    );
    let run = |turn: TurnId, kind: TurnOriginKind| {
        let fx = &fx;
        let key = key.clone();
        let approver = approver.clone();
        async move {
            crate::peers::turn_origin::record_turn_origin(&key, &turn, origin(kind, None));
            let registry = turn_registry(fx, &key, &turn.0.to_string()).await;
            octos_agent::tools::TOOL_APPROVAL_CTX
                .scope(
                    approver,
                    registry.execute_with_context(&call_ctx("c1"), "news_list", &json!({})),
                )
                .await
                .unwrap()
        }
    };
    // The person is in the app: their turn is attended.
    let ran = run(TurnId::new(), TurnOriginKind::Person).await;
    assert!(ran.success, "{}", ran.output);
    // The app's own run on the same session is a background run.
    let refused = run(TurnId::new(), TurnOriginKind::App).await;
    assert!(
        !refused.success && refused.output.contains("background"),
        "{}",
        refused.output
    );
    crate::peers::turn_origin::clear_turn_origin(&key);
    drop(ws);
    crate::peers::host_tools::set_host_route(&peers_root(&fx), "news", 0, Arc::new(|_, _| false));
    assert_eq!(host.await.unwrap().len(), 1);
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
    for attempt in 0..1000 {
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
    tokio::time::timeout(std::time::Duration::from_secs(20), llm.entered.notified())
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
    for _ in 0..1000 {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_let_only_the_host_open_a_sharing_context_and_label_its_turns() {
    use octos_core::ui_protocol::TurnOriginKind;
    let llm = Arc::new(RecordingLlm::default());
    let mut e = e2e_fixture(llm.clone(), json!({ "tools": [] })).await;
    let host_frames = collect_frames(e.rx.take().unwrap());
    let share = json!({"share_history": {}});
    let kind = |result: Result<Value, RpcError>| result.unwrap_err().data.unwrap()["kind"].clone();

    // No connection, another connection of the profile, an external client.
    assert_eq!(
        kind(open_context_from(&e, None, "ui-1", share.clone())),
        "share_history_host_only"
    );
    let (foreign, foreign_rx) = ws_connection_for_test(64);
    let foreign_frames = collect_frames(foreign_rx);
    assert_eq!(
        kind(open_context_from(&e, Some(&foreign), "ui-1", share.clone())),
        "share_history_host_only"
    );
    let (external, _external_rx) = external_ws(64);
    assert_eq!(
        kind(open_context_from(
            &e,
            Some(&external),
            "ui-1",
            share.clone()
        )),
        "share_history_host_only"
    );
    // Bad settings.
    assert!(
        open_context_from(
            &e,
            Some(&e.ws),
            "ui-1",
            json!({"share_history": {"last_n": 0}})
        )
        .is_err()
    );
    assert!(
        open_context_from(
            &e,
            Some(&e.ws),
            "ui-1",
            json!({"share_history": {"all": true}})
        )
        .is_err()
    );
    // The caps.
    let capped = open_context_from(
        &e,
        Some(&e.ws),
        "ui-1",
        json!({"share_history": {"last_n": 500, "max_bytes": 10_000_000}}),
    )
    .unwrap();
    assert_eq!(
        capped["share_history"],
        json!({"last_n": 50, "max_bytes": 65536})
    );
    // Fixed at creation: a re-open restates it.
    assert_eq!(
        kind(open_context_from(&e, Some(&e.ws), "ui-1", json!({}))),
        "peer_binding_mismatch"
    );
    let reopened = open_context_from(
        &e,
        Some(&e.ws),
        "ui-1",
        json!({"share_history": {"last_n": 500, "max_bytes": 10_000_000}}),
    )
    .unwrap();
    assert_eq!(reopened["created"], false);
    // A plain context stays as it was: no sharing, no origin.
    let plain = open_context_from(&e, None, "mini-a", json!({})).unwrap();
    assert_eq!(plain["share_history"], Value::Null);

    let context: SessionKey = serde_json::from_value(capped["session_id"].clone()).unwrap();
    let plain: SessionKey = serde_json::from_value(plain["session_id"].clone()).unwrap();
    let active: SharedActiveTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let origin_of = |kind, label| Some(origin(kind, label));
    // Never the system agent's lane...
    assert!(
        !start_turn(
            &e,
            &e.ws,
            &active,
            "o1",
            &context,
            &TurnId::new(),
            "hi",
            origin_of(TurnOriginKind::SystemAgent, None)
        )
        .await
    );
    assert_eq!(
        error_kind_of(&host_frames, "o1").await,
        "turn_origin_mismatch"
    );
    // ...an origin only from the host...
    assert!(
        !start_turn(
            &e,
            &foreign,
            &active,
            "o2",
            &context,
            &TurnId::new(),
            "hi",
            origin_of(TurnOriginKind::Person, None)
        )
        .await
    );
    assert_eq!(
        error_kind_of(&foreign_frames, "o2").await,
        "turn_origin_host_only"
    );
    // ...and none on a plain context.
    assert!(
        !start_turn(
            &e,
            &e.ws,
            &active,
            "o3",
            &plain,
            &TurnId::new(),
            "hi",
            origin_of(TurnOriginKind::Person, None)
        )
        .await
    );
    assert_eq!(
        error_kind_of(&host_frames, "o3").await,
        "turn_origin_not_allowed"
    );
    // The host may name the person, or say the app speaks.
    let app_turn = TurnId::new();
    start_turn_when_free(
        &e,
        &active,
        "o4",
        &context,
        &app_turn,
        "refresh",
        origin_of(TurnOriginKind::Person, Some("Ada")),
    )
    .await;
    let result = wait_result(&e, &app_turn).await;
    assert!(result.contains("\norigin: person\n"), "{result}");
    let request = llm.requests.lock().unwrap()[0].clone();
    assert!(
        request.contains("[from the person: Ada] refresh"),
        "{request}"
    );
    // The plain context's turn writes no blackboard round.
    let plain_turn = TurnId::new();
    start_turn_when_free(&e, &active, "o5", &plain, &plain_turn, "mini", None).await;
    // (Slow runners: wait up to 20 s for the plain turn to reach the model.)
    for _ in 0..1000 {
        if llm.requests.lock().unwrap().len() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let request = llm.requests.lock().unwrap()[1].clone();
    assert!(!request.contains("<shared_history"), "{request}");
    assert!(
        files_containing(&e.data_dir.join("peers/news"), &plain_turn.0.to_string()).is_empty(),
        "a plain context leaves no round"
    );
}

#[tokio::test]
async fn should_number_concurrent_rounds_of_both_lanes_without_collisions() {
    use crate::peers::app_binding::{PeerContextBinding, write_context_binding};
    let fx = fixture().await;
    prepare_news(&fx).await;
    let peers = peers_root(&fx);
    write_context_binding(
        &peers,
        "news",
        "ui-1",
        &PeerContextBinding {
            version: 1,
            cwd: fx.apps.join("news/contexts/ui-1"),
            memory_namespace: "app/news/acct-1/ctx-ui-1".into(),
            closed: false,
            share_history: Some(crate::peers::shared_history::ShareHistory {
                last_n: 20,
                max_bytes: 16 * 1024,
            }),
            read_parent: false,
        },
    )
    .unwrap();
    let context = crate::peers::app_binding::context_session_key(&fx.system, "news", "ui-1");
    let turns: Vec<TurnId> = (0..16).map(|_| TurnId::new()).collect();
    std::thread::scope(|scope| {
        for turn in &turns {
            let (peers, context) = (&peers, &context);
            scope.spawn(move || {
                write_sharing_context_round(
                    peers,
                    context,
                    turn,
                    TurnTerminalOutcome::Completed,
                    "hi",
                )
            });
        }
    });
    let dir = peers.join("news");
    let mut seen = std::collections::HashSet::new();
    for round in 1..=16 {
        let body = std::fs::read_to_string(dir.join(format!("result-{round}.md"))).unwrap();
        assert!(body.contains(&format!("\nturn: {round}\n")), "{body}");
        let turn = body
            .lines()
            .find_map(|line| line.strip_prefix("turn_id: "))
            .unwrap()
            .to_owned();
        assert!(seen.insert(turn), "round {round} reused a turn");
    }
    assert!(!dir.join("result-17.md").exists());
    let index = std::fs::read_to_string(dir.join("turns.txt")).unwrap();
    let rounds: Vec<u32> = index
        .lines()
        .map(|line| line.split(' ').next().unwrap().parse().unwrap())
        .collect();
    assert_eq!(rounds, (1..=16).collect::<Vec<_>>(), "{index}");
    let latest = std::fs::read_to_string(dir.join("result.md")).unwrap();
    assert!(latest.contains("\nturn: 16\n"), "{latest}");
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
        let usage = octos_llm::TokenUsage {
            input_tokens: 1,
            output_tokens: 1,
            ..Default::default()
        };
        let reply = |content: &str| octos_llm::ChatResponse {
            content: Some(content.into()),
            reasoning_content: None,
            tool_calls: Vec::new(),
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: usage.clone(),
            provider_index: None,
        };
        if own.contains("SEND_IT") && !after_tool {
            return Ok(octos_llm::ChatResponse {
                content: Some("Sending the mail now.".into()),
                reasoning_content: None,
                tool_calls: vec![octos_core::ToolCall {
                    id: "call_send".into(),
                    name: "mail_send".into(),
                    arguments: json!({"draft_id": "d-secret-args"}),
                    metadata: None,
                }],
                stop_reason: octos_llm::StopReason::ToolUse,
                usage,
                provider_index: None,
            });
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
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while std::time::Instant::now() < deadline {
        let pending = contracts.approvals.pending_for_session(session);
        if !pending.is_empty() {
            return pending;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the turn never asked for an approval");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_show_the_peer_session_a_persons_turn_in_progress() {
    let llm = Arc::new(LaneLlm::default());
    let mut e = e2e_fixture(llm.clone(), json!({ "tools": [] })).await;
    let frames = collect_frames(e.rx.take().unwrap());
    let peer = shared_peer(&e);
    let context = sharing_context(&e, "ui-1");
    let active: SharedActiveTurns = Arc::new(TokioMutex::new(HashMap::new()));

    // The person's turn is running (it holds the model).
    let person_turn = TurnId::new();
    assert!(
        start_turn(
            &e,
            &e.ws,
            &active,
            "p1",
            &context,
            &person_turn,
            "HOLD this for me",
            None
        )
        .await
    );
    tokio::time::timeout(std::time::Duration::from_secs(60), llm.held.notified())
        .await
        .expect("the person's turn reached the model");

    // The system agent's turn sees it.
    let input_turn = peer_input_turn_id(&e, &frames, "CHECK_MAIL", "call_1").await;
    start_turn_when_free(&e, &active, "s1", &peer, &input_turn, "CHECK_MAIL", None).await;
    wait_result(&e, &input_turn).await;
    let request = llm.request_with("[from the system agent] CHECK_MAIL");
    assert!(
        request.contains("<shared_history lane=\"person\" read_only=\"true\">"),
        "{request}"
    );
    assert!(
        request.contains("[from the person] HOLD this for me\n"),
        "{request}"
    );
    assert!(
        request.contains("[turn status] still running\n</shared_history>"),
        "{request}"
    );

    llm.release.notify_one();
    wait_result(&e, &person_turn).await;
    assert_no_block_persisted(&e);
}

// ---------------------------------------------------------------------------
// UPCR-2026-034 `read_parent`: a request context's read-only view of its
// peer's folder (#2603).
// ---------------------------------------------------------------------------

/// The peer folder of the e2e fixture's News peer, with account data, another
/// context's private file, and the runtime of context `id` opened with
/// `read_parent` (or without it).
async fn read_parent_context(
    e: &E2e,
    id: &str,
    read_parent: bool,
) -> (PathBuf, Arc<crate::runtime::SessionRuntime>) {
    let extra = if read_parent {
        json!({"read_parent": true})
    } else {
        json!({})
    };
    let opened = open_context_from(e, Some(&e.ws), id, extra).expect("open");
    assert_eq!(opened["read_parent"], read_parent);
    let peer_root = dunce::canonicalize(e.data_dir.parent().unwrap().join("apps/news")).unwrap();
    std::fs::write(peer_root.join("threads.md"), "ACCOUNT-THREADS").unwrap();
    std::fs::create_dir_all(peer_root.join("contexts/other")).unwrap();
    std::fs::write(peer_root.join("contexts/other/private.md"), "OTHER-CONTEXT").unwrap();
    let key = SessionKey(opened["session_id"].as_str().unwrap().to_owned());
    let profile = e.state.profiles.get("dev").unwrap().clone();
    let runtime = crate::runtime::SessionRuntime::bootstrap(&profile, key, None)
        .await
        .expect("bootstrap the context");
    (peer_root, runtime)
}

async fn run_tool(
    runtime: &crate::runtime::SessionRuntime,
    tool: &str,
    args: Value,
) -> octos_agent::tools::ToolResult {
    let ctx = octos_agent::tools::ToolContext {
        tool_id: "t".to_owned(),
        session_scope: runtime.agent.session_scope().cloned(),
        ..octos_agent::tools::ToolContext::zero()
    };
    runtime
        .tools
        .execute_with_context(&ctx, tool, &args)
        .await
        .expect("the tool runs")
}

#[tokio::test]
async fn should_read_the_peer_folder_but_not_another_context_when_the_context_has_read_parent() {
    let llm = Arc::new(RecordingLlm::default());
    let e = e2e_fixture(llm, json!({ "tools": [] })).await;
    let (peer, rt) = read_parent_context(&e, "ui-read", true).await;
    let path = |p: &str| peer.join(p).to_string_lossy().into_owned();

    // Reads of the peer's folder.
    let read = run_tool(&rt, "read_file", json!({"path": path("threads.md")})).await;
    assert!(
        read.success && read.output.contains("ACCOUNT-THREADS"),
        "{}",
        read.output
    );
    let listed = run_tool(&rt, "list_dir", json!({"path": path("")})).await;
    assert!(
        listed.success && listed.output.contains("threads.md"),
        "{}",
        listed.output
    );
    assert!(
        !listed.output.contains("contexts"),
        "the contexts folder is hidden: {}",
        listed.output
    );
    let found = run_tool(
        &rt,
        "glob",
        json!({"pattern": format!("{}/**/*.md", path(""))}),
    )
    .await;
    assert!(found.output.contains("threads.md"), "{}", found.output);
    assert!(!found.output.contains("private.md"), "{}", found.output);
    let grep = run_tool(
        &rt,
        "grep",
        json!({"pattern": "ACCOUNT-THREADS|OTHER-CONTEXT", "path": path("")}),
    )
    .await;
    assert!(grep.output.contains("ACCOUNT-THREADS"), "{}", grep.output);
    assert!(!grep.output.contains("OTHER-CONTEXT"), "{}", grep.output);

    // Another context's folder stays unreadable.
    let other = run_tool(
        &rt,
        "read_file",
        json!({"path": path("contexts/other/private.md")}),
    )
    .await;
    assert!(
        !other.success && !other.output.contains("OTHER-CONTEXT"),
        "{}",
        other.output
    );
    let other_dir = run_tool(&rt, "list_dir", json!({"path": path("contexts/other")})).await;
    assert!(!other_dir.success, "{}", other_dir.output);
    let other_glob = run_tool(
        &rt,
        "glob",
        json!({"pattern": format!("{}/*.md", path("contexts/other"))}),
    )
    .await;
    assert!(
        !other_glob.output.contains("private.md"),
        "{}",
        other_glob.output
    );
    let other_grep = run_tool(
        &rt,
        "grep",
        json!({"pattern": "OTHER-CONTEXT", "path": path("contexts/other")}),
    )
    .await;
    assert!(
        !other_grep.output.contains("OTHER-CONTEXT"),
        "{}",
        other_grep.output
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(peer.join("contexts/other"), peer.join("link")).unwrap();
        let via_link = run_tool(&rt, "read_file", json!({"path": path("link/private.md")})).await;
        assert!(
            !via_link.output.contains("OTHER-CONTEXT"),
            "{}",
            via_link.output
        );
    }

    // Writes stay in the context's own folder.
    let write = run_tool(
        &rt,
        "write_file",
        json!({"path": path("threads.md"), "content": "overwritten"}),
    )
    .await;
    assert!(!write.success, "{}", write.output);
    assert_eq!(
        std::fs::read_to_string(peer.join("threads.md")).unwrap(),
        "ACCOUNT-THREADS"
    );
    let edit = run_tool(
        &rt,
        "edit_file",
        json!({"path": path("threads.md"), "old_string": "ACCOUNT", "new_string": "X"}),
    )
    .await;
    assert!(!edit.success, "{}", edit.output);
    let own = run_tool(
        &rt,
        "write_file",
        json!({"path": "notes.md", "content": "mine"}),
    )
    .await;
    assert!(own.success, "{}", own.output);
    assert_eq!(
        std::fs::read_to_string(peer.join("contexts/ui-read/notes.md")).unwrap(),
        "mine"
    );

    // The shell's sandbox carries the same view.
    let view = rt.sandbox.read_only_view.as_ref().expect("sandbox view");
    assert_eq!(view.root, peer);
    assert_eq!(view.excluded, vec![peer.join("contexts")]);
}

#[tokio::test]
async fn should_keep_the_context_fenced_to_its_own_folder_when_read_parent_is_not_set() {
    let llm = Arc::new(RecordingLlm::default());
    let e = e2e_fixture(llm, json!({ "tools": [] })).await;
    let (peer, rt) = read_parent_context(&e, "ui-plain", false).await;
    let read = run_tool(
        &rt,
        "read_file",
        json!({"path": peer.join("threads.md").to_string_lossy()}),
    )
    .await;
    assert!(
        !read.success && !read.output.contains("ACCOUNT-THREADS"),
        "{}",
        read.output
    );
    assert!(rt.sandbox.read_only_view.is_none());
    assert!(
        rt.agent
            .session_scope()
            .is_some_and(|scope| scope.read_only_view().is_none())
    );
}

#[tokio::test]
async fn should_refuse_read_parent_when_the_caller_is_not_the_peers_host() {
    let llm = Arc::new(RecordingLlm::default());
    let e = e2e_fixture(llm, json!({ "tools": [] })).await;
    let flag = json!({"read_parent": true});
    let kind = |result: Result<Value, RpcError>| result.unwrap_err().data.unwrap()["kind"].clone();
    assert_eq!(
        kind(open_context_from(&e, None, "ui-1", flag.clone())),
        "read_parent_host_only"
    );
    let (foreign, _foreign_rx) = ws_connection_for_test(64);
    assert_eq!(
        kind(open_context_from(&e, Some(&foreign), "ui-1", flag.clone())),
        "read_parent_host_only"
    );
    let (external, _external_rx) = external_ws(64);
    assert_eq!(
        kind(open_context_from(&e, Some(&external), "ui-1", flag.clone())),
        "read_parent_host_only"
    );
    // Nothing was recorded by the refusals.
    assert!(
        crate::peers::app_binding::read_context_binding(&e.data_dir.join("peers"), "news", "ui-1")
            .is_none()
    );
}

#[tokio::test]
async fn should_refuse_a_reopen_when_it_changes_read_parent() {
    let llm = Arc::new(RecordingLlm::default());
    let e = e2e_fixture(llm, json!({ "tools": [] })).await;
    let flag = json!({"read_parent": true});
    let opened = open_context_from(&e, Some(&e.ws), "ui-1", flag.clone()).unwrap();
    assert_eq!(opened["created"], true);
    let again = open_context_from(&e, Some(&e.ws), "ui-1", flag).unwrap();
    assert_eq!(again["created"], false);
    assert_eq!(again["read_parent"], true);
    let changed = open_context_from(&e, Some(&e.ws), "ui-1", json!({})).unwrap_err();
    assert_eq!(changed.data.unwrap()["kind"], "peer_binding_mismatch");
    open_context_from(&e, Some(&e.ws), "ui-2", json!({})).unwrap();
    let widened =
        open_context_from(&e, Some(&e.ws), "ui-2", json!({"read_parent": true})).unwrap_err();
    assert_eq!(widened.data.unwrap()["kind"], "peer_binding_mismatch");
}
