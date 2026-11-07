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

#[tokio::test]
async fn ingress_lifetime_observer_requires_own_tool_call_and_retains_read() {
    let session = SessionKey(format!("_main:api:observer-{}", uuid::Uuid::new_v4()));
    let (entered, _entered_rx) = mpsc::unbounded_channel();
    let llm = GatedToolLlm {
        entered,
        release: tokio::sync::Semaphore::new(0),
        calls: AtomicUsize::new(0),
        total_calls: AtomicUsize::new(0),
        tool_results: AtomicUsize::new(0),
        observed_tools: std::sync::Mutex::new(Vec::new()),
        path: "unused".into(),
        request_marker: format!("gated-read-request:{}", session.0),
        tool_call_id: format!("call_{}", uuid::Uuid::new_v4().simple()),
    };
    let mut result = Message::user("TOOL_RAN_AFTER_CLOSE");
    result.role = MessageRole::Tool;
    result.tool_call_id = Some("another-tests-tool-call".into());
    let config = octos_llm::ChatConfig::default();
    // Matching text under a different tool call must never satisfy the test.
    octos_llm::LlmProvider::chat(&llm, std::slice::from_ref(&result), &[], &config)
        .await
        .unwrap();
    assert_eq!(llm.tool_results.load(Ordering::SeqCst), 0);
    result.tool_call_id = Some(llm.tool_call_id.clone());
    for _ in 0..2 {
        octos_llm::LlmProvider::chat(&llm, std::slice::from_ref(&result), &[], &config)
            .await
            .unwrap();
        assert_eq!(llm.tool_results.load(Ordering::SeqCst), 1);
    }
    finish_foreign_continuation(&llm).await;
    assert_eq!(llm.tool_results.load(Ordering::SeqCst), 1);
    assert_eq!(llm.calls.load(Ordering::SeqCst), 0);
}

struct TestServer(tokio::task::JoinHandle<()>);
impl Drop for TestServer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Debug, Clone, Copy)]
enum DisconnectCase {
    RevokedIngress,
    ValidIngress,
    OrdinaryBrowser,
}

async fn real_ws_turn_cleanup(case: DisconnectCase) {
    use crate::contracts::scope::{ApprovalScopeKind, MatchKey};

    let ingress = !matches!(case, DisconnectCase::OrdinaryBrowser);
    let revoke_ingress = matches!(case, DisconnectCase::RevokedIngress);
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
    if revoke_ingress {
        assert!(state.work_secret_store.revoke_token(&token).unwrap());
        // The documented revalidation boundary is the next client request.
        socket.send(ClientMessage::Text(json!({"jsonrpc":"2.0", "id":"after-revoke", "method":"turn/state/get", "params":{"session_id":session,"turn_id":turn}}).to_string().into())).await.unwrap();
    } else {
        if ingress {
            assert!(state.work_secret_store.validate(&session.0, &token).is_ok());
        }
        socket.send(ClientMessage::Close(None)).await.unwrap();
    }
    let close = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(frame) = socket.next().await {
            if let ClientMessage::Close(close) = frame.unwrap() {
                return close;
            }
        }
        panic!("expected WS close");
    })
    .await
    .unwrap();
    if revoke_ingress {
        let close = close.unwrap();
        assert_eq!(u16::from(close.code), 1008);
        assert_eq!(close.reason, "session_ingress_revoked");
    }
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
async fn revoked_work_secret_aborts_turn_and_pending_prompts_over_real_ws() {
    real_ws_turn_cleanup(DisconnectCase::RevokedIngress).await;
}

#[tokio::test]
async fn ordinary_ws_disconnect_preserves_turn_and_pending_prompts_over_real_ws() {
    real_ws_turn_cleanup(DisconnectCase::OrdinaryBrowser).await;
}

#[tokio::test]
async fn valid_work_secret_disconnect_aborts_turn_and_pending_prompts_over_real_ws() {
    real_ws_turn_cleanup(DisconnectCase::ValidIngress).await;
}

// The real spawn/three-tool scenarios also run against an isolated serve with
// a stub provider. Here the transport fixture seeds supervisors from completed
// turns, so teardown must work even with an empty connection-turn registry.
async fn real_ws_finished_parent_background_cleanup(revoke: bool) {
    let dir = tempfile::tempdir().unwrap();
    // This fixture expects zero model calls of any kind. Give its queue drain
    // a private profile rather than sharing other tests' _main continuations.
    let profile = format!("finished-parent-{}", uuid::Uuid::new_v4());
    let session = SessionKey::with_profile(&profile, "api", "finished-parent");
    let (entered, _entered_rx) = mpsc::unbounded_channel();
    let llm = Arc::new(GatedToolLlm {
        entered,
        release: tokio::sync::Semaphore::new(0),
        calls: AtomicUsize::new(0),
        total_calls: AtomicUsize::new(0),
        tool_results: AtomicUsize::new(0),
        observed_tools: std::sync::Mutex::new(Vec::new()),
        request_marker: format!("gated-read-request:{}", session.0),
        tool_call_id: format!("call_{}", uuid::Uuid::new_v4().simple()),
        path: dir.path().join("unused"),
    });
    let (mut state, _profile) = state_with_profile_llm(dir.path(), &profile, llm.clone()).await;
    Arc::get_mut(&mut state).unwrap().work_secret_store = Arc::new(
        octos_agent::bridge::work_secret::WorkSecretGrantStore::new(dir.path()),
    );
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
            Some(profile.clone()),
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
    let contracts = contract_stores();
    let parent_turn = TurnId::new();
    let mut approval = contracts
        .approvals
        .request_runtime(ApprovalRequestedEvent::generic(
            session.clone(),
            ApprovalId::new(),
            parent_turn.clone(),
            "exec",
            "Pending child",
            "A parked background tool",
        ));
    let mut question = contracts
        .user_questions
        .request_runtime(sample_pending_question(
            session.clone(),
            QuestionId::new(),
            parent_turn.clone(),
        ));
    use crate::contracts::scope::{ApprovalScopeKind, MatchKey};
    assert!(contracts.scopes.record(
        &session,
        ApprovalScopeKind::ApproveForSession,
        MatchKey::Session,
        ApprovalDecision::Approve
    ));
    let supervisor = Arc::new(octos_agent::TaskSupervisor::new());
    let _work = contracts
        .background_work
        .register(&session, &profile, &parent_turn, &supervisor);
    let task = supervisor.register("spawn", "child", Some(&session.0));
    supervisor.mark_running(&task);
    let (cancelled_tx, cancelled_rx) = oneshot::channel();
    let cancelled_tx = std::sync::Mutex::new(Some(cancelled_tx));
    supervisor.set_on_change(move |task| {
        if task.status == octos_agent::TaskStatus::Cancelled
            && let Some(tx) = cancelled_tx.lock().unwrap().take()
        {
            let _ = tx.send(());
        }
    });
    let mut request = format!("ws://{addr}/v1/session_ingress/ws/{}", session.0)
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    // HTTP upgrade can precede bind_ingress_generation. Wait for a scoped RPC
    // through the read loop before testing post-bind revocation/disconnection.
    socket
        .send(ClientMessage::Text(
            json!({
                "jsonrpc": "2.0", "id": "bound", "method": "approval/scopes/list",
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
                let response: serde_json::Value = serde_json::from_str(&text).unwrap();
                if response["id"] == "bound" {
                    assert!(response.get("error").is_none(), "{response}");
                    let scopes = response["result"]["scopes"].as_array().unwrap();
                    assert_eq!(scopes.len(), 1);
                    assert_eq!(scopes[0]["session_id"], session.0);
                    return;
                }
            }
        }
        panic!("socket closed before ingress bind completed");
    })
    .await
    .expect("post-bind scoped RPC response");
    if revoke {
        assert!(state.work_secret_store.revoke_token(&token).unwrap());
        socket.send(ClientMessage::Text("{}".into())).await.unwrap();
    } else {
        socket.send(ClientMessage::Close(None)).await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(frame) = socket.next().await {
            if let ClientMessage::Close(close) = frame.unwrap() {
                if revoke {
                    assert_eq!(u16::from(close.unwrap().code), 1008);
                }
                return;
            }
        }
        panic!("expected close frame");
    })
    .await
    .unwrap();
    if revoke {
        tokio::time::timeout(Duration::from_secs(5), cancelled_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            supervisor.get_task(&task).unwrap().status,
            octos_agent::TaskStatus::Cancelled
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while !contracts.approvals.pending_for_session(&session).is_empty()
                || !contracts
                    .user_questions
                    .pending_for_session(&session)
                    .is_empty()
                || contracts
                    .scopes
                    .lookup(&session, "exec", &parent_turn)
                    .is_some()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            approval.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
        assert!(matches!(
            question.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
    } else {
        assert_eq!(
            supervisor.get_task(&task).unwrap().status,
            octos_agent::TaskStatus::Running
        );
        assert!(state.work_secret_store.validate(&session.0, &token).is_ok());
        assert!(matches!(
            approval.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            question.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        contracts
            .approvals
            .cancel_pending_for_turn(&session, &parent_turn, "test_cleanup");
        contracts
            .user_questions
            .cancel_pending_for_turn(&session, &parent_turn, "test_cleanup");
        contracts.scopes.evict_session(&session);
    }
    assert_eq!(
        llm.total_calls.load(Ordering::SeqCst),
        0,
        "no parent or automatic turn needed for cleanup"
    );
}

#[tokio::test]
async fn revoked_ingress_cancels_background_work_after_parent_finished_over_real_ws() {
    real_ws_finished_parent_background_cleanup(true).await;
}

#[tokio::test]
async fn valid_ingress_disconnect_preserves_finished_parent_background_work_over_real_ws() {
    real_ws_finished_parent_background_cleanup(false).await;
}

#[tokio::test]
async fn revoked_ingress_old_socket_cannot_claim_new_generation_continuation() {
    use crate::autonomy::master_continuation_scheduler::MasterContinuationRequest;
    use crate::contracts::background_work::GrantStamp;

    let profile = "ingress-stale-continuation";
    let session = SessionKey::with_profile(profile, "api", "fresh-work");
    let temp = tempfile::tempdir().unwrap();
    let provider = Arc::new(AppuiContinuationLlm::new("fresh continuation completed"));
    let (state, _) = state_with_profile_llm(temp.path(), profile, provider.clone()).await;
    let contracts = Arc::new(UiProtocolContractStores::default());
    let old = contracts.background_work.observe(
        &session,
        profile,
        Some(GrantStamp {
            token_hash: "old".into(),
            active: true,
        }),
    );
    let fresh = contracts.background_work.observe(
        &session,
        profile,
        Some(GrantStamp {
            token_hash: "fresh".into(),
            active: true,
        }),
    );
    assert_ne!(old.generation, fresh.generation);
    let orchestrator = default_agent_orchestrator();
    orchestrator.enqueue_continuation_for_test(MasterContinuationRequest::new(
        "stale-ingress-regression",
        session.0.clone(),
        profile.to_owned(),
        MasterContinuationReason::External("fresh-grant".into()),
        std::time::SystemTime::now(),
    ));
    let (mut stale_ws, _stale_rx) = ws_connection_for_test(64);
    stale_ws.ingress_generation = Some(old.generation);
    stale_ws._ingress_scope_lifetime = Some(old.lifetime);
    let (mut fresh_ws, _fresh_rx) = ws_connection_for_test(64);
    fresh_ws.ingress_generation = Some(fresh.generation);
    fresh_ws._ingress_scope_lifetime = Some(fresh.lifetime);
    let ledger = Arc::new(UiProtocolLedger::new(64));
    let active = Arc::new(TokioMutex::new(HashMap::new()));
    let stale_turns = Arc::new(TokioMutex::new(HashMap::new()));
    assert!(
        !maybe_spawn_appui_master_continuation_runner(
            &stale_ws,
            &state,
            &ledger,
            &contracts,
            &active,
            &stale_turns,
            session.clone(),
            session.clone(),
            profile.into(),
            ConnectionUiFeatures::stdio_defaults(),
        )
        .await
    );
    assert!(stale_turns.lock().await.is_empty());
    assert_eq!(
        orchestrator.pending_continuation_count_for_session_for_test(&session, profile),
        1
    );
    assert_eq!(provider.call_count.load(Ordering::Relaxed), 0);
    let fresh_turns = Arc::new(TokioMutex::new(HashMap::new()));
    assert!(
        maybe_spawn_appui_master_continuation_runner(
            &fresh_ws,
            &state,
            &ledger,
            &contracts,
            &active,
            &fresh_turns,
            session.clone(),
            session.clone(),
            profile.into(),
            ConnectionUiFeatures::stdio_defaults(),
        )
        .await
    );
    wait_for_appui_continuation(provider.as_ref()).await;
    assert_eq!(provider.call_count.load(Ordering::Relaxed), 1);
    assert_eq!(
        orchestrator.pending_continuation_count_for_session_for_test(&session, profile),
        0
    );
}

/// T-0309: a corrupt work-secret grants file must not strip ordinary browser
/// turns of their existing capability, and must not be misread as a
/// revocation that retires/cancels legitimate background work.
#[tokio::test]
async fn corrupt_grants_file_preserves_browser_turns_and_background_work() {
    let dir = tempfile::tempdir().unwrap();
    let profile = format!("corrupt-grants-{}", uuid::Uuid::new_v4());
    let session = SessionKey::with_profile(&profile, "api", "corrupt-grants");
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
        path: dir.path().join("unused"),
    });
    finish_foreign_continuation(&llm).await;
    let (mut state, _profile) = state_with_profile_llm(dir.path(), &profile, llm.clone()).await;
    Arc::get_mut(&mut state).unwrap().work_secret_store = Arc::new(
        octos_agent::bridge::work_secret::WorkSecretGrantStore::new(dir.path()),
    );
    let contracts = contract_stores();
    // Seed legitimate background work owned by a prior grant generation.
    let parent_turn = TurnId::new();
    let supervisor = Arc::new(octos_agent::TaskSupervisor::new());
    let _work = contracts
        .background_work
        .register(&session, &profile, &parent_turn, &supervisor);
    let task = supervisor.register("spawn", "child", Some(&session.0));
    supervisor.mark_running(&task);
    // Corrupt the grants file (invalid JSON).
    std::fs::write(dir.path().join("work_secrets.json"), "{ not valid json").unwrap();

    // Drive an ordinary browser turn/start (no ingress scope).
    let active_turns = active_turns_registry();
    let connection_turns: SharedConnectionTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let ledger = event_ledger(&state).await;
    let (ws, _rx) = ws_connection_for_test(64);
    let turn = TurnId::new();
    handle_turn_start(
        &ws,
        &state,
        &ledger,
        &contracts,
        &active_turns,
        &connection_turns,
        None,
        None,
        ConnectionUiFeatures::stdio_defaults(),
        "turn".into(),
        TurnStartParams {
            session_id: session.clone(),
            turn_id: turn.clone(),
            input: vec![InputItem::Text {
                text: format!("Run the gated file read {}", llm.request_marker),
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

    // The browser turn is admitted and reaches the provider despite the
    // corrupt grants file.
    tokio::time::timeout(Duration::from_secs(10), entered_rx.recv())
        .await
        .expect("browser turn must be admitted despite a corrupt grants file")
        .unwrap();
    // The pre-existing background work was NOT retired/cancelled by treating
    // the unreadable file as a revocation.
    assert_eq!(
        supervisor.get_task(&task).unwrap().status,
        octos_agent::TaskStatus::Running,
        "a corrupt grants file must not cancel legitimate background work"
    );
}

/// T-0309 negative control: the SECRET path stays fail-closed on a corrupt
/// grants file — an ingress connection is rejected, never let through.
#[tokio::test]
async fn corrupt_grants_file_rejects_ingress_bind_over_real_ws() {
    let dir = tempfile::tempdir().unwrap();
    let profile = format!("corrupt-ingress-{}", uuid::Uuid::new_v4());
    let session = SessionKey::with_profile(&profile, "api", "corrupt-ingress");
    let (entered, _entered_rx) = mpsc::unbounded_channel();
    let llm = Arc::new(GatedToolLlm {
        entered,
        release: tokio::sync::Semaphore::new(0),
        calls: AtomicUsize::new(0),
        total_calls: AtomicUsize::new(0),
        tool_results: AtomicUsize::new(0),
        observed_tools: std::sync::Mutex::new(Vec::new()),
        request_marker: format!("gated-read-request:{}", session.0),
        tool_call_id: format!("call_{}", uuid::Uuid::new_v4().simple()),
        path: dir.path().join("unused"),
    });
    finish_foreign_continuation(&llm).await;
    let (mut state, _profile) = state_with_profile_llm(dir.path(), &profile, llm.clone()).await;
    Arc::get_mut(&mut state).unwrap().work_secret_store = Arc::new(
        octos_agent::bridge::work_secret::WorkSecretGrantStore::new(dir.path()),
    );
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
            Some(profile.clone()),
        )
        .unwrap();
    // Corrupt the grants file AFTER issuing the token.
    std::fs::write(dir.path().join("work_secrets.json"), "{ not valid json").unwrap();
    let app = crate::api::build_router(state.clone());
    let _server = TestServer(tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    }));
    let mut request = format!("ws://{addr}/v1/session_ingress/ws/{}", session.0)
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    // The route validates the work-secret before upgrading; a corrupt grants
    // file fails that validation closed, so the upgrade itself is rejected.
    let error = tokio_tungstenite::connect_async(request)
        .await
        .expect_err("ingress upgrade must be rejected on a corrupt grants file");
    assert!(
        error.to_string().contains("401"),
        "expected an auth rejection, got {error}"
    );
    assert_eq!(llm.calls.load(Ordering::SeqCst), 0, "no turn admitted");
}
