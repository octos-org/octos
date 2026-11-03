//! Ordinary WS transport loss is not cancellation of admitted server work.
use super::*;

struct DetachedTurnLlm {
    entered: mpsc::UnboundedSender<Vec<Message>>,
    release: tokio::sync::Semaphore,
}

#[async_trait::async_trait]
impl octos_llm::LlmProvider for DetachedTurnLlm {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> eyre::Result<octos_llm::ChatResponse> {
        self.entered.send(messages.to_vec()).unwrap();
        self.release.acquire().await.unwrap().forget();
        Ok(octos_llm::ChatResponse {
            content: Some("Completed detached work".into()),
            reasoning_content: None,
            tool_calls: Vec::new(),
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: octos_llm::TokenUsage::default(),
            provider_index: None,
        })
    }
    fn model_id(&self) -> &str {
        "detached-turn-stub"
    }
    fn provider_name(&self) -> &str {
        "stub"
    }
}

#[tokio::test]
async fn ws_detached_turns_keep_running_and_persist_prompts_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let (entered, mut provider_calls) = mpsc::unbounded_channel();
    let llm = Arc::new(DetachedTurnLlm {
        entered,
        release: tokio::sync::Semaphore::new(0),
    });
    let (state, profile) = state_with_profile_llm(dir.path(), MAIN_PROFILE_ID, llm.clone()).await;
    let ledger = Arc::new(UiProtocolLedger::new(128));
    let observer = message_commit_observer(ledger.clone());
    octos_bus::session::set_scoped_message_commit_observer(&profile.data_dir, &observer);
    let contracts = Arc::new(UiProtocolContractStores::default());
    let active = active_turns_registry();
    let connections = Arc::new(TokioMutex::new(HashMap::new()));
    let (ws, rx) = ws_connection_for_test(256);
    ws.update_live_features(ConnectionUiFeatures::stdio_defaults());
    let mut turns = Vec::new();
    for foreground in [false, true] {
        let turn = TurnId::new();
        let session = SessionKey::new("api", &format!("detach-{}", turn.0));
        let text = format!(
            "Run the {} request",
            if foreground {
                "foreground"
            } else {
                "background"
            }
        );
        handle_turn_start(
            &ws,
            &state,
            &ledger,
            &contracts,
            &active,
            &connections,
            None,
            None,
            ConnectionUiFeatures::stdio_defaults(),
            format!("start-{foreground}"),
            TurnStartParams {
                session_id: session.clone(),
                turn_id: turn.clone(),
                input: vec![InputItem::Text { text: text.clone() }],
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
        turns.push((session, turn, text));
    }
    // Disconnect immediately after acceptance: initial turn/started delivery
    // may race this close as well as later model/tool/terminal notifications.
    ws.mark_failed();
    drop(rx);
    let registered = connections.lock().await.clone();
    detach_connection_turns(&active, &connections, &contracts.scopes).await;
    assert!(connections.lock().await.is_empty());
    for _ in 0..2 {
        let prompt = tokio::time::timeout(Duration::from_secs(10), provider_calls.recv())
            .await
            .expect("both accepted turns must reach the provider")
            .unwrap();
        assert_eq!(
            prompt
                .iter()
                .filter(|m| m.role == MessageRole::User)
                .count(),
            1,
            "early transcript persistence must not duplicate the model input"
        );
    }
    for (session, turn, text) in &turns {
        let registry = active.lock().await;
        let entry = registry
            .get(session)
            .expect("running work remains registered");
        assert_eq!(entry.turn_id, *turn);
        assert!(matches!(*entry.state.lock().await, TurnState::Active));
        drop(registry);
        let messages = cached_session_messages(&state, &profile, session).await;
        assert_eq!(
            messages.iter().filter(|m| m.content == *text).count(),
            1,
            "reconnect can hydrate the prompt while the model is blocked"
        );
    }
    llm.release.add_permits(2);
    for (session, turn, text) in &turns {
        tokio::time::timeout(
            Duration::from_secs(10),
            registered[session].finished.clone(),
        )
        .await
        .expect("detached turn finishes without a consumer");
        let messages = cached_session_messages(&state, &profile, session).await;
        assert_eq!(
            messages
                .iter()
                .filter(|m| m.role == MessageRole::User && m.content == *text)
                .count(),
            1
        );
        assert!(messages.iter().any(|m| m.role == MessageRole::Assistant && m.content == "Completed detached work"));
        let rows = ledger
            .replay_after(
                session,
                Some(&UiCursor {
                    stream: session.to_string(),
                    seq: 0,
                }),
            )
            .unwrap();
        assert!(rows.iter().any(|row| matches!(&row.event,
            UiProtocolLedgerEvent::Notification(UiNotification::TurnCompleted(event)) if event.turn_id == *turn)));
        assert!(!rows.iter().any(|row| matches!(
            &row.event,
            UiProtocolLedgerEvent::Notification(UiNotification::TurnError(_))
        )));
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let registry = active.lock().await;
            let retired = turns
                .iter()
                .all(|(session, _, _)| !registry.contains_key(session));
            drop(registry);
            if retired {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached dispatches retire after their outer tasks finish");
}

#[tokio::test]
async fn ws_detached_cleanup_cannot_remove_a_replacement_with_reused_turn_id() {
    let session = SessionKey("local:detached-replacement".into());
    let turn = TurnId::new();
    let (finish, finished) = oneshot::channel::<()>();
    let old = tokio::spawn(async move {
        let _ = finished.await;
    });
    let old_entry = test_active_turn(turn.clone(), old.abort_handle());
    let registered = ConnectionTurn {
        turn_id: turn.clone(),
        state: old_entry.state.clone(),
        finished: async move {
            let _ = old.await;
        }
        .boxed()
        .shared(),
    };
    let joined = registered.finished.clone();
    let active = Arc::new(TokioMutex::new(HashMap::from([(
        session.clone(),
        old_entry,
    )])));
    let connections = Arc::new(TokioMutex::new(HashMap::from([(
        session.clone(),
        registered,
    )])));
    detach_connection_turns(&active, &connections, &ScopePolicy::default()).await;
    let replacement = tokio::spawn(std::future::pending::<()>());
    let entry = test_active_turn(turn.clone(), replacement.abort_handle());
    let replacement_state = entry.state.clone();
    active.lock().await.insert(session.clone(), entry);
    finish.send(()).unwrap();
    joined.await;
    tokio::task::yield_now().await;
    assert!(Arc::ptr_eq(
        &active.lock().await[&session].state,
        &replacement_state
    ));
    replacement.abort();
}

#[test]
fn ws_server_owned_lifetime_does_not_relax_external_or_stdio_ownership() {
    let (ws, _rx) = ws_connection_for_test(8);
    assert!(ws.server_owns_turns());
    ws.set_external(true);
    assert!(!ws.server_owns_turns());
    let (tx, _rx) = std::sync::mpsc::sync_channel(8);
    assert!(!WsConnection::new_stdio(tx).server_owns_turns());
}

#[tokio::test]
async fn ws_detached_approval_and_question_wait_for_an_authorized_reconnect() {
    let temp = tempfile::tempdir().unwrap();
    let session = g1_system_session();
    let state = g1_state(temp.path(), &session).await;
    let ledger = Arc::new(UiProtocolLedger::new(64));
    let contracts = Arc::new(UiProtocolContractStores::default());
    let (gone, rx) = ws_connection_for_test(64);
    gone.mark_failed();
    drop(rx);
    let (approval, approval_id) =
        g1_raise_approval(&gone, &ledger, &contracts, &state, &session).await;
    let (question, question_id) =
        g1_ask_question(&gone, &ledger, &contracts, &state, &session).await;
    assert!(!approval.is_finished());
    assert!(!question.is_finished());
    let (reconnected, mut rx) = ws_connection_for_test(64);
    assert_eq!(
        g1_opened_pending(
            &state,
            &ledger,
            &contracts,
            reconnected.connection_id,
            &session
        )
        .await,
        vec![approval_id.clone()]
    );
    assert_eq!(
        g1_opened_questions(
            &state,
            &ledger,
            &contracts,
            reconnected.connection_id,
            &session
        )
        .await,
        vec![question_id.clone()]
    );
    handle_approval_respond(
        &reconnected,
        &state,
        &ledger,
        &contracts,
        None,
        None,
        "reconnect-approval".into(),
        ApprovalRespondParams::new(session.clone(), approval_id, ApprovalDecision::Approve),
    )
    .await;
    let frame = recv_rpc_response_with_id(&mut rx, "reconnect-approval").await;
    assert!(frame.get("error").is_none(), "{frame}");
    assert_eq!(approval.await.unwrap(), ToolApprovalDecision::Approve);
    handle_user_question_respond(
        &reconnected,
        &state,
        &contracts,
        None,
        None,
        "reconnect-question".into(),
        UserQuestionRespondParams::new(session, question_id, g1_answer()),
    )
    .await;
    let frame = recv_rpc_response_with_id(&mut rx, "reconnect-question").await;
    assert!(frame.get("error").is_none(), "{frame}");
    assert!(matches!(
        question.await.unwrap(),
        octos_agent::UserQuestionOutcome::Answered(_)
    ));
}

#[tokio::test]
async fn ws_dispatched_media_and_reused_turn_ids_preserve_each_new_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionKey("api:early-media-and-reuse".into());
    let sessions = Arc::new(TokioMutex::new(
        octos_bus::SessionManager::open(dir.path()).unwrap(),
    ));
    let context = Arc::new(StdMutex::new(ContextManager::new(
        session.to_string(),
        None,
    )));
    for text in ["first request", "second request with the reused client id"] {
        let mut message = Message::user(text);
        message.media = vec!["/workspace/retained.png".into()];
        assert!(
            postturn_context_batch::persist_turn_prompt(
                &sessions,
                &session,
                "reused-thread",
                message,
                dir.path(),
                &context,
                false,
            )
            .await
            .unwrap()
            .is_some()
        );
    }
    postturn_context_batch::persist_interrupted_turn_prompt(
        &sessions,
        &session,
        "reused-thread",
        "second request with the reused client id",
        dir.path(),
        &context,
    )
    .await;
    let mut reopened = octos_bus::SessionManager::open(dir.path()).unwrap();
    let rows = &reopened.get_or_create(&session).await.messages;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].content, "second request with the reused client id");
    assert!(
        rows.iter()
            .all(|row| row.media == ["/workspace/retained.png"])
    );
}
