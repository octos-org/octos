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
        let read = stub_tool_call(&self.tool_call_id, "read_file", json!({"path": self.path}));
        let calls = if first { vec![read] } else { Vec::new() };
        Ok(stub_reply(Some("finished"), calls))
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
