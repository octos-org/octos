//! Exercise the real WS route and teardown, including work-secret validation.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_tungstenite::tungstenite::{Message as ClientMessage, client::IntoClientRequest};

struct GatedToolLlm {
    entered: mpsc::UnboundedSender<()>,
    release: tokio::sync::Semaphore,
    calls: AtomicUsize,
    total_calls: AtomicUsize,
    tool_results: AtomicUsize,
    observed_tools: std::sync::Mutex<Vec<String>>,
    path: std::path::PathBuf,
    request_marker: String,
    tool_call_id: String,
}

#[async_trait::async_trait]
impl octos_llm::LlmProvider for GatedToolLlm {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> eyre::Result<octos_llm::ChatResponse> {
        self.total_calls.fetch_add(1, Ordering::SeqCst);
        // The process-wide continuation queue can deliver another test's
        // [system-internal] request before ours. Finish it without taking our
        // gate or issuing our file tool in that other session's scope.
        let own_request = messages
            .iter()
            .rfind(|m| m.role == MessageRole::User)
            .is_some_and(|m| m.content.contains(&self.request_marker));
        let first = own_request && self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        self.observed_tools.lock().unwrap().extend(
            messages
                .iter()
                .filter(|m| m.role == MessageRole::Tool)
                .map(|m| format!("{:?}: {}", m.tool_call_id, m.content)),
        );
        // Only the uniquely identified fixture read counts. Later unrelated
        // requests must not erase it, nor may repeated histories count twice.
        self.tool_results.fetch_max(
            messages
                .iter()
                .filter(|m| {
                    m.role == MessageRole::Tool
                        && m.tool_call_id.as_deref() == Some(self.tool_call_id.as_str())
                        && m.content.contains("TOOL_RAN_AFTER_CLOSE")
                })
                .count(),
            Ordering::SeqCst,
        );
        if first {
            self.entered.send(()).unwrap();
            self.release.acquire().await.unwrap().forget();
        }
        Ok(octos_llm::ChatResponse {
            content: Some("finished".into()),
            reasoning_content: None,
            tool_calls: if first {
                vec![octos_core::ToolCall {
                    id: self.tool_call_id.clone(),
                    name: "read_file".into(),
                    arguments: json!({"path": self.path}),
                    metadata: None,
                }]
            } else {
                Vec::new()
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
        "ingress-lifetime-stub"
    }
    fn provider_name(&self) -> &str {
        "stub"
    }
}

async fn finish_foreign_continuation(llm: &GatedToolLlm) {
    let foreign = [Message::user(
        "[system-internal] A supervised child agent finished. Group: agent-group:_main:local:test:master",
    )];
    let response = tokio::time::timeout(
        Duration::from_secs(1),
        octos_llm::LlmProvider::chat(llm, &foreign, &[], &octos_llm::ChatConfig::default()),
    )
    .await
    .expect("foreign continuation must finish without taking the fixture gate")
    .unwrap();
    assert!(response.tool_calls.is_empty());
    assert_eq!(response.stop_reason, octos_llm::StopReason::EndTurn);
}

struct TestServer(tokio::task::JoinHandle<()>);
impl Drop for TestServer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Debug, Clone, Copy)]
enum DisconnectCase {
    ValidIngress,
    OrdinaryBrowser,
}

async fn real_ws_turn_cleanup(case: DisconnectCase) {
    use crate::contracts::scope::{ApprovalScopeKind, MatchKey};

    let ingress = !matches!(case, DisconnectCase::OrdinaryBrowser);
    let dir = tempfile::tempdir().unwrap();
    let session = SessionKey(format!(
        "{MAIN_PROFILE_ID}:api:ingress-lifetime-{}",
        uuid::Uuid::new_v4()
    ));
    let (entered, mut entered_rx) = mpsc::unbounded_channel();
    let llm = Arc::new(GatedToolLlm {
        entered,
        release: tokio::sync::Semaphore::new(0),
        calls: AtomicUsize::new(0),
        total_calls: AtomicUsize::new(0),
        tool_results: AtomicUsize::new(0),
        observed_tools: std::sync::Mutex::new(Vec::new()),
        request_marker: format!("gated-read-request:{}", session.0),
        tool_call_id: format!("call_{}", uuid::Uuid::new_v4().simple()),
        path: dir
            .path()
            .join("profiles")
            .join(MAIN_PROFILE_ID)
            .join("data/users")
            .join(octos_bus::session::encode_path_component(
                session.base_key(),
            ))
            .join("workspace/marker.txt"),
    });
    finish_foreign_continuation(&llm).await;
    assert_eq!(llm.calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        entered_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    let (mut state, _profile) =
        state_with_profile_llm(dir.path(), MAIN_PROFILE_ID, llm.clone()).await;
    std::fs::create_dir_all(llm.path.parent().unwrap()).unwrap();
    std::fs::write(&llm.path, "TOOL_RAN_AFTER_CLOSE\n").unwrap();
    Arc::get_mut(&mut state).unwrap().work_secret_store = Arc::new(
        octos_agent::bridge::work_secret::WorkSecretGrantStore::new(dir.path()),
    );
    assert!(
        state.host_managed.is_none(),
        "ordinary deployment is the regression"
    );
    let turn = TurnId::new();
    let token = uuid::Uuid::new_v4().to_string();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    state
        .work_secret_store
        .issue(
            &session.0,
            &token,
            format!("http://{addr}"),
            chrono::Duration::minutes(5),
            Some(MAIN_PROFILE_ID.into()),
        )
        .unwrap();
    let app = crate::api::build_router(state.clone());
    let _server = TestServer(tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    }));
    let route = if ingress {
        format!("/v1/session_ingress/ws/{}", session.0)
    } else {
        "/api/ui-protocol/ws".into()
    };
    let mut request = format!("ws://{addr}{route}").into_client_request().unwrap();
    if ingress {
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
    }
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    socket.send(ClientMessage::Text(json!({
        "jsonrpc": "2.0", "id": "start", "method": "turn/start",
        "params": {"session_id": session, "turn_id": turn, "input": [{"kind":"text", "text":format!("Run the gated file read {}", llm.request_marker)}]},
    }).to_string().into())).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered_rx.recv())
        .await
        .expect("turn reaches provider")
        .unwrap();
    let active = active_turns_registry();
    let (turn_state, abort) = {
        let active = active.lock().await;
        let entry = active.get(&session).expect("accepted turn registered");
        (entry.state.clone(), entry.abort.clone())
    };
    let contracts = contract_stores();
    let approval_id = ApprovalId::new();
    let question_id = QuestionId::new();
    let mut approval = contracts
        .approvals
        .request_runtime(ApprovalRequestedEvent::generic(
            session.clone(),
            approval_id.clone(),
            turn.clone(),
            "exec",
            "Pending approval",
            "A parked tool",
        ));
    let mut question = contracts
        .user_questions
        .request_runtime(sample_pending_question(
            session.clone(),
            question_id.clone(),
            turn.clone(),
        ));
    assert!(contracts.scopes.record(
        &session,
        ApprovalScopeKind::ApproveForSession,
        MatchKey::Session,
        ApprovalDecision::Approve
    ));
    if ingress {
        assert!(state.work_secret_store.validate(&session.0, &token).is_ok());
    }
    socket.send(ClientMessage::Close(None)).await.unwrap();
    let _close = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(frame) = socket.next().await {
            if let ClientMessage::Close(close) = frame.unwrap() {
                return close;
            }
        }
        panic!("expected WS close");
    })
    .await
    .unwrap();
    // Scope eviction is the final step of the existing abort path, and also
    // happens on ordinary detach. Wait for teardown before releasing the LLM.
    tokio::time::timeout(Duration::from_secs(10), async {
        while contracts.scopes.lookup(&session, "exec", &turn).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("connection cleanup clears remembered grants");
    if matches!(case, DisconnectCase::ValidIngress) {
        assert!(
            state.work_secret_store.validate(&session.0, &token).is_ok(),
            "disconnect must not revoke a still-valid grant"
        );
    }
    let approval_cancelled = matches!(
        approval.try_recv(),
        Err(oneshot::error::TryRecvError::Closed)
    );
    let question_cancelled = matches!(
        question.try_recv(),
        Err(oneshot::error::TryRecvError::Closed)
    );
    llm.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(10), async {
        while !abort.is_finished() || active.lock().await.contains_key(&session) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("turn task must finish");
    let reason = match *turn_state.lock().await {
        TurnState::Terminal(reason) => reason,
        ref other => panic!("expected terminal state: {other:?}"),
    };
    let ledger = event_ledger(&state).await;
    let rows = ledger
        .replay_after(
            &session,
            Some(&UiCursor {
                stream: session.0.clone(),
                seq: 0,
            }),
        )
        .unwrap();
    let cancelled_approvals = rows.iter().filter(|row| matches!(&row.event, UiProtocolLedgerEvent::Notification(UiNotification::ApprovalCancelled(e)) if e.approval_id == approval_id)).count();
    let cancelled_questions = rows.iter().filter(|row| matches!(&row.event, UiProtocolLedgerEvent::Notification(UiNotification::UserQuestionCancelled(e)) if e.question_id == question_id)).count();
    // Clean fixture-only pending waiters, even on the red implementation.
    contracts
        .approvals
        .cancel_pending_for_turn(&session, &turn, "test_cleanup");
    contracts
        .user_questions
        .cancel_pending_for_turn(&session, &turn, "test_cleanup");
    drop(socket);
    eprintln!(
        "case={case:?} terminal={reason:?} tool_results={} approval_cancelled={approval_cancelled} question_cancelled={question_cancelled}",
        llm.tool_results.load(Ordering::SeqCst)
    );
    finish_foreign_continuation(&llm).await;
    if ingress {
        assert_eq!(reason, TerminalReason::Interrupted);
        assert_eq!(llm.calls.load(Ordering::SeqCst), 1);
        assert_eq!(llm.tool_results.load(Ordering::SeqCst), 0);
        assert!(approval_cancelled && question_cancelled);
        assert_eq!((cancelled_approvals, cancelled_questions), (1, 1));
    } else {
        assert_eq!(reason, TerminalReason::Completed);
        assert_eq!(
            llm.tool_results.load(Ordering::SeqCst),
            1,
            "provider calls={}, observed tool messages={:?}",
            llm.calls.load(Ordering::SeqCst),
            llm.observed_tools.lock().unwrap()
        );
        assert!(!approval_cancelled && !question_cancelled);
        assert_eq!((cancelled_approvals, cancelled_questions), (0, 0));
    }
}

#[tokio::test]
async fn ordinary_ws_disconnect_preserves_turn_and_pending_prompts_over_real_ws() {
    real_ws_turn_cleanup(DisconnectCase::OrdinaryBrowser).await;
}

#[tokio::test]
async fn valid_work_secret_disconnect_aborts_turn_and_pending_prompts_over_real_ws() {
    real_ws_turn_cleanup(DisconnectCase::ValidIngress).await;
}

type IngressSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct BackgroundProbe {
    _work: Arc<crate::contracts::background_work::BackgroundWork>,
    supervisor: Arc<octos_agent::TaskSupervisor>,
    task_id: String,
    turn_id: TurnId,
}

impl BackgroundProbe {
    fn new(
        contracts: &UiProtocolContractStores,
        session: &SessionKey,
        profile: &str,
        generation: u64,
    ) -> Self {
        let turn_id = TurnId::new();
        let work = contracts
            .background_work
            .admit(session, profile, &turn_id, Some(generation))
            .expect("background work joins the observed generation");
        let supervisor = Arc::new(octos_agent::TaskSupervisor::new());
        work.attach(&supervisor);
        let task_id = supervisor.register("spawn", "prebind-child", Some(&session.0));
        assert!(!task_id.is_empty());
        supervisor.mark_running(&task_id);
        Self {
            _work: work,
            supervisor,
            task_id,
            turn_id,
        }
    }

    fn status(&self) -> octos_agent::TaskStatus {
        self.supervisor.get_task(&self.task_id).unwrap().status
    }
}

async fn connect_ingress(
    addr: std::net::SocketAddr,
    session: &SessionKey,
    token: &str,
) -> IngressSocket {
    let mut request = format!("ws://{addr}/v1/session_ingress/ws/{}", session.0)
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    tokio_tungstenite::connect_async(request).await.unwrap().0
}

async fn await_ingress_bind(socket: &mut IngressSocket, session: &SessionKey) {
    let id = format!("bind-{}", uuid::Uuid::new_v4().simple());
    socket
        .send(ClientMessage::Text(
            json!({
                "jsonrpc": "2.0", "id": id, "method": "approval/scopes/list",
                "params": {"session_id": session},
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(frame) = socket.next().await {
            if let ClientMessage::Text(text) = frame.unwrap() {
                let response: Value = serde_json::from_str(&text).unwrap();
                if response["id"] == id {
                    assert!(response.get("error").is_none(), "{response}");
                    return;
                }
            }
        }
        panic!("socket closed before its scoped RPC completed")
    })
    .await
    .expect("scoped RPC proves ingress bind completed");
}

async fn expect_revoked_close(socket: &mut IngressSocket) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(frame) = socket.next().await {
            if let ClientMessage::Close(close) = frame.unwrap() {
                let close = close.expect("policy close carries a reason");
                assert_eq!(u16::from(close.code), 1008);
                assert_eq!(close.reason, "session_ingress_revoked");
                return;
            }
        }
        panic!("socket ended without its revocation close")
    })
    .await
    .expect("revoked ingress close");
}

fn issue_ingress_grant(
    state: &AppState,
    addr: std::net::SocketAddr,
    session: &SessionKey,
    profile: &str,
    token: &str,
) {
    state
        .work_secret_store
        .issue(
            &session.0,
            token,
            format!("http://{addr}"),
            chrono::Duration::minutes(5),
            Some(profile.to_owned()),
        )
        .unwrap();
}

/// A pre-bind 1008 must retire the presented grant's old generation, without
/// letting that delayed socket cancel a newer grant or post-revocation browser work.
#[tokio::test]
async fn revoked_work_secret_prebind_cancels_only_matching_generation_over_real_ws() {
    use crate::contracts::scope::{ApprovalScopeKind, MatchKey};

    let dir = tempfile::tempdir().unwrap();
    let profile = format!("prebind-revocation-{}", uuid::Uuid::new_v4().simple());
    let (entered, _entered_rx) = mpsc::unbounded_channel();
    let provider = Arc::new(GatedToolLlm {
        entered,
        release: tokio::sync::Semaphore::new(0),
        calls: AtomicUsize::new(0),
        total_calls: AtomicUsize::new(0),
        tool_results: AtomicUsize::new(0),
        observed_tools: std::sync::Mutex::new(Vec::new()),
        request_marker: format!("no-model-call:{}", uuid::Uuid::new_v4().simple()),
        tool_call_id: format!("call_{}", uuid::Uuid::new_v4().simple()),
        path: dir.path().join("unused"),
    });
    let (mut state, _profile) =
        state_with_profile_llm(dir.path(), &profile, provider.clone()).await;
    Arc::get_mut(&mut state).unwrap().work_secret_store = Arc::new(
        octos_agent::bridge::work_secret::WorkSecretGrantStore::new(dir.path()),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = crate::api::build_router(state.clone());
    let _server = TestServer(tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    }));
    let contracts = contract_stores();
    let ledger = event_ledger(&state).await;
    let active = active_turns_registry();

    // Hold admission between the route's first validation and the socket bind's
    // second validation. The HTTP upgrade completes while bind waits on this lock.
    let session = SessionKey::with_profile(&profile, "api", "prebind-revoked");
    let token = uuid::Uuid::new_v4().to_string();
    issue_ingress_grant(&state, addr, &session, &profile, &token);
    let mut old_socket = connect_ingress(addr, &session, &token).await;
    await_ingress_bind(&mut old_socket, &session).await;
    let old_generation = contracts
        .background_work
        .peek(&session, &profile)
        .generation;
    let old = BackgroundProbe::new(&contracts, &session, &profile, old_generation);
    let mut approval = contracts
        .approvals
        .request_runtime(ApprovalRequestedEvent::generic(
            session.clone(),
            ApprovalId::new(),
            old.turn_id.clone(),
            "exec",
            "Pending pre-bind child",
            "A parked background tool",
        ));
    let mut question = contracts
        .user_questions
        .request_runtime(sample_pending_question(
            session.clone(),
            QuestionId::new(),
            old.turn_id.clone(),
        ));
    assert!(contracts.scopes.record(
        &session,
        ApprovalScopeKind::ApproveForSession,
        MatchKey::Session,
        ApprovalDecision::Approve
    ));
    let automatic_task = tokio::spawn(std::future::pending::<()>());
    let mut automatic = test_active_turn(old.turn_id.clone(), automatic_task.abort_handle());
    automatic.profile_id = profile.clone();
    let automatic_state = automatic.state.clone();
    active.lock().await.insert(session.clone(), automatic);

    let admission = active.lock().await;
    let mut rejected = connect_ingress(addr, &session, &token).await;
    assert!(state.work_secret_store.revoke_token(&token).unwrap());
    drop(admission);
    expect_revoked_close(&mut rejected).await;

    assert_eq!(old.status(), octos_agent::TaskStatus::Cancelled);
    assert!(matches!(
        approval.try_recv(),
        Err(oneshot::error::TryRecvError::Closed)
    ));
    assert!(matches!(
        question.try_recv(),
        Err(oneshot::error::TryRecvError::Closed)
    ));
    assert!(
        contracts
            .scopes
            .lookup(&session, "exec", &old.turn_id)
            .is_none()
    );
    assert!(matches!(
        &*automatic_state.lock().await,
        TurnState::Interrupting {
            origin: InterruptOrigin::IngressRevoked,
            ..
        }
    ));
    active.lock().await.remove(&session);
    automatic_task.abort();

    // The established post-bind path retains its existing cleanup semantics.
    let post_session = SessionKey::with_profile(&profile, "api", "postbind-revoked");
    let post_token = uuid::Uuid::new_v4().to_string();
    issue_ingress_grant(&state, addr, &post_session, &profile, &post_token);
    let mut post_socket = connect_ingress(addr, &post_session, &post_token).await;
    await_ingress_bind(&mut post_socket, &post_session).await;
    let post_generation = contracts
        .background_work
        .peek(&post_session, &profile)
        .generation;
    let post = BackgroundProbe::new(&contracts, &post_session, &profile, post_generation);
    assert!(state.work_secret_store.revoke_token(&post_token).unwrap());
    post_socket
        .send(ClientMessage::Text("{}".into()))
        .await
        .unwrap();
    expect_revoked_close(&mut post_socket).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while post.status() != octos_agent::TaskStatus::Cancelled {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("post-bind revocation still cancels old background work");

    // Replacement retires the old generation itself, even if a public-store
    // caller reuses the same token. The delayed socket must not retire the
    // replacement or its automatic continuation a second time.
    for reuse_token in [false, true] {
        let new_session =
            SessionKey::with_profile(&profile, "api", &format!("replacement-grant-{reuse_token}"));
        let old_token = uuid::Uuid::new_v4().to_string();
        let new_token = if reuse_token {
            old_token.clone()
        } else {
            uuid::Uuid::new_v4().to_string()
        };
        issue_ingress_grant(&state, addr, &new_session, &profile, &old_token);
        let mut previous_socket = connect_ingress(addr, &new_session, &old_token).await;
        await_ingress_bind(&mut previous_socket, &new_session).await;
        let previous_generation = contracts
            .background_work
            .peek(&new_session, &profile)
            .generation;
        let previous =
            BackgroundProbe::new(&contracts, &new_session, &profile, previous_generation);
        let mut admission = active.lock().await;
        let mut stale = connect_ingress(addr, &new_session, &old_token).await;
        issue_ingress_grant(&state, addr, &new_session, &profile, &new_token);
        let replacement =
            observe_background_grant(&state, &contracts, &ledger, &new_session, &profile, None)
                .unwrap();
        let fresh =
            BackgroundProbe::new(&contracts, &new_session, &profile, replacement.generation);
        assert_eq!(previous.status(), octos_agent::TaskStatus::Cancelled);
        let fresh_automatic_task = tokio::spawn(std::future::pending::<()>());
        let mut fresh_automatic =
            test_active_turn(fresh.turn_id.clone(), fresh_automatic_task.abort_handle());
        fresh_automatic.profile_id = profile.clone();
        let fresh_automatic_state = fresh_automatic.state.clone();
        admission.insert(new_session.clone(), fresh_automatic);
        drop(admission);
        expect_revoked_close(&mut stale).await;
        let mut fresh_socket = connect_ingress(addr, &new_session, &new_token).await;
        await_ingress_bind(&mut fresh_socket, &new_session).await;
        assert_eq!(fresh.status(), octos_agent::TaskStatus::Running);
        assert!(matches!(
            &*fresh_automatic_state.lock().await,
            TurnState::Active
        ));
        active.lock().await.remove(&new_session);
        fresh_automatic_task.abort();
        contracts
            .background_work
            .revoke_generation(&new_session, &profile, replacement.generation);
    }

    // Browser admission observes the inactive grant, retires its old generation,
    // then joins a new one. A delayed socket presenting that same revoked token
    // must not cancel the browser's new work merely because the hash matches.
    let browser_session = SessionKey::with_profile(&profile, "api", "browser-after-revoke");
    let browser_token = uuid::Uuid::new_v4().to_string();
    issue_ingress_grant(&state, addr, &browser_session, &profile, &browser_token);
    let mut browser_old_socket = connect_ingress(addr, &browser_session, &browser_token).await;
    await_ingress_bind(&mut browser_old_socket, &browser_session).await;
    let browser_old_generation = contracts
        .background_work
        .peek(&browser_session, &profile)
        .generation;
    let browser_old = BackgroundProbe::new(
        &contracts,
        &browser_session,
        &profile,
        browser_old_generation,
    );
    let admission = active.lock().await;
    let mut browser_stale = connect_ingress(addr, &browser_session, &browser_token).await;
    assert!(
        state
            .work_secret_store
            .revoke_token(&browser_token)
            .unwrap()
    );
    let browser = observe_background_grant(
        &state,
        &contracts,
        &ledger,
        &browser_session,
        &profile,
        None,
    )
    .unwrap();
    let browser_work =
        BackgroundProbe::new(&contracts, &browser_session, &profile, browser.generation);
    assert_eq!(browser_old.status(), octos_agent::TaskStatus::Cancelled);
    drop(admission);
    expect_revoked_close(&mut browser_stale).await;
    assert_eq!(browser_work.status(), octos_agent::TaskStatus::Running);

    assert_eq!(provider.total_calls.load(Ordering::SeqCst), 0);
    contracts
        .background_work
        .revoke_generation(&browser_session, &profile, browser.generation);
}
