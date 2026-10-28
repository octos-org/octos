//! Dispatched prompts survive every abnormal turn termination (#2881).
use super::*;

#[tokio::test]
async fn should_commit_disconnected_prompt_before_terminal_without_duplicates() {
    for already_persisted in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionKey("local:disconnect-prompt".into());
        let turn = TurnId::new();
        let text = "Keep the dispatched prompt after disconnect";
        let ledger = Arc::new(UiProtocolLedger::new(16));
        let observer = message_commit_observer(ledger.clone());
        octos_bus::session::set_scoped_message_commit_observer(dir.path(), &observer);
        let sessions = Arc::new(TokioMutex::new(
            octos_bus::SessionManager::open(dir.path()).unwrap(),
        ));
        let context = Arc::new(StdMutex::new(ContextManager::new(
            session.to_string(),
            None,
        )));
        context.lock().unwrap().record_message(&Message::user(text));
        if already_persisted {
            postturn_context_batch::persist_interrupted_turn_prompt(
                &sessions,
                &session,
                &turn.0.to_string(),
                text,
                dir.path(),
                &context,
            )
            .await;
        }
        // The outer task need not have run its drop guard yet when cleanup
        // starts. Retaining the child handle must cancel AND join the child.
        let child = tokio::spawn(std::future::pending::<()>());
        let child_abort = child.abort_handle();
        let child_finished = async move {
            assert!(child.await.unwrap_err().is_cancelled());
        }
        .boxed()
        .shared();
        let prompt = Arc::new(DispatchedTurnPrompt {
            text: text.into(),
            sessions,
            thread_id: turn.0.to_string(),
            data_dir: dir.path().into(),
            context,
            agent_abort: child_abort.clone(),
            agent_finished: child_finished,
        });
        let outer = tokio::spawn(std::future::pending::<()>());
        let mut entry = test_active_turn(turn.clone(), outer.abort_handle());
        entry.dispatched_prompt = Some(prompt);
        let connection = ConnectionTurn {
            turn_id: turn.clone(),
            state: entry.state.clone(),
        };
        let active = Arc::new(TokioMutex::new(HashMap::from([(session.clone(), entry)])));
        let connections = Arc::new(TokioMutex::new(HashMap::from([(
            session.clone(),
            connection,
        )])));
        tokio::time::timeout(
            Duration::from_secs(5),
            abort_connection_turns(
                &active,
                &connections,
                &ScopePolicy::default(),
                &ledger,
                &PendingApprovalStore::default(),
                &PendingQuestionStore::default(),
            ),
        )
        .await
        .expect("disconnect must join the cancelled child before committing");
        assert!(child_abort.is_finished());
        assert!(outer.await.unwrap_err().is_cancelled());
        let rows = ledger
            .replay_after(
                &session,
                Some(&UiCursor {
                    stream: session.to_string(),
                    seq: 0,
                }),
            )
            .unwrap();
        let prompt_rows: Vec<_> = rows.iter().enumerate().filter(|(_, row)| {
            matches!(&row.event, UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(envelope))
                if matches!(&envelope.envelope.payload, PayloadV2::UserMessage { text: body, .. } if body == text))
        }).collect();
        assert_eq!(
            prompt_rows.len(),
            1,
            "the prompt must be projected exactly once"
        );
        let terminal_at = rows.iter().position(|row| {
            matches!(&row.event, UiProtocolLedgerEvent::Notification(UiNotification::TurnError(error))
                if error.code == "connection_closed")
        }).unwrap();
        assert!(
            prompt_rows[0].0 < terminal_at,
            "prompt must precede terminal"
        );
        let mut reopened = octos_bus::SessionManager::open(dir.path()).unwrap();
        let history = &reopened.get_or_create(&session).await.messages;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content, text);
        assert_eq!(
            history[0].thread_id.as_deref(),
            Some(turn.0.to_string().as_str())
        );
    }
}

struct TerminatedPromptLlm {
    panic: bool,
}

#[async_trait::async_trait]
impl octos_llm::LlmProvider for TerminatedPromptLlm {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> eyre::Result<octos_llm::ChatResponse> {
        assert!(!self.panic, "fixture agent task panic");
        eyre::bail!("fixture provider error")
    }

    fn model_id(&self) -> &str {
        "terminated-prompt-stub"
    }
    fn provider_name(&self) -> &str {
        "stub"
    }
}

#[tokio::test]
async fn should_commit_terminated_prompt_when_provider_errors_or_agent_panics() {
    for panic in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (state, profile) = state_with_profile_llm(
            dir.path(),
            MAIN_PROFILE_ID,
            Arc::new(TerminatedPromptLlm { panic }),
        )
        .await;
        let turn = TurnId::new();
        let session = SessionKey::new("api", &format!("terminated-prompt-{}", turn.0));
        let text = "Retain this prompt when the agent fails";
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let observer = message_commit_observer(ledger.clone());
        octos_bus::session::set_scoped_message_commit_observer(&profile.data_dir, &observer);
        let contracts = Arc::new(UiProtocolContractStores::default());
        let active = active_turns_registry();
        let connections = Arc::new(TokioMutex::new(HashMap::new()));
        let (ws, mut rx) = ws_connection_for_test(128);
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
            "start-failing".into(),
            TurnStartParams {
                session_id: session.clone(),
                turn_id: turn.clone(),
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
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let frame = recv_rpc_json(&mut rx).await;
                assert!(frame.get("error").is_none(), "{frame}");
                if frame["params"]["payload"]["type"] == "turn_terminal" {
                    let code = frame["params"]["payload"]["data"]["error"]["code"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    assert_eq!(
                        code,
                        if panic {
                            "terminal_lost"
                        } else {
                            "turn_failed"
                        }
                    );
                    break;
                }
            }
        })
        .await
        .expect("the failed task must emit a terminal");
        let messages = cached_session_messages(&state, &profile, &session).await;
        assert_eq!(
            messages
                .iter()
                .filter(|m| m.role == MessageRole::User && m.content == text)
                .count(),
            1
        );
        let rows = ledger
            .replay_after(
                &session,
                Some(&UiCursor {
                    stream: session.to_string(),
                    seq: 0,
                }),
            )
            .unwrap();
        let prompt_at = rows.iter().position(|row| {
            matches!(&row.event, UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(event))
                if matches!(&event.envelope.payload, PayloadV2::UserMessage { text: body, .. } if body == text))
        }).expect("durable user envelope");
        let terminal_at = rows
            .iter()
            .position(|row| {
                matches!(
                    &row.event,
                    UiProtocolLedgerEvent::Notification(UiNotification::TurnError(_))
                )
            })
            .expect("durable terminal");
        assert!(prompt_at < terminal_at);
        abort_connection_turns(
            &active,
            &connections,
            &contracts.scopes,
            &ledger,
            &contracts.approvals,
            &contracts.user_questions,
        )
        .await;
        let messages = cached_session_messages(&state, &profile, &session).await;
        assert_eq!(
            messages
                .iter()
                .filter(|m| m.role == MessageRole::User && m.content == text)
                .count(),
            1,
            "closing after the error must not duplicate its prompt"
        );
    }
}

#[tokio::test]
async fn should_finish_in_progress_prompt_commit_before_cancelling_agent() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionKey("local:prompt-commit-race".into());
    let thread = "prompt-commit-thread";
    let sessions = Arc::new(TokioMutex::new(
        octos_bus::SessionManager::open(dir.path()).unwrap(),
    ));
    let context = Arc::new(StdMutex::new(ContextManager::new(
        session.to_string(),
        None,
    )));
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let writer_sessions = sessions.clone();
    let writer_session = session.clone();
    let child = tokio::spawn(async move {
        let mut sessions = writer_sessions.lock().await;
        entered_tx.send(()).unwrap();
        release_rx.await.unwrap();
        sessions
            .add_message_with_seq(
                &writer_session,
                pre_stamp_turn_thread_id(Message::user("in-progress prompt"), thread),
            )
            .await
            .unwrap();
    });
    let abort = child.abort_handle();
    let finished = async move {
        let _ = child.await;
    }
    .boxed()
    .shared();
    let prompt = DispatchedTurnPrompt {
        text: "in-progress prompt".into(),
        sessions: sessions.clone(),
        thread_id: thread.into(),
        data_dir: dir.path().into(),
        context,
        agent_abort: abort.clone(),
        agent_finished: finished,
    };
    entered_rx.await.unwrap();
    let cleanup = prompt.abort_and_wait(None);
    tokio::pin!(cleanup);
    assert!(futures::poll!(&mut cleanup).is_pending());
    tokio::task::yield_now().await;
    assert!(
        !abort.is_finished(),
        "a writer holding the session lock must not be aborted"
    );
    release_tx
        .send(())
        .expect("the committing child must still be alive");
    tokio::time::timeout(Duration::from_secs(5), &mut cleanup)
        .await
        .unwrap();
    prompt.persist(&session).await;
    let mut reopened = octos_bus::SessionManager::open(dir.path()).unwrap();
    let messages = &reopened.get_or_create(&session).await.messages;
    assert_eq!(
        messages.len(),
        1,
        "the in-flight append and cleanup must not both write"
    );
    assert_eq!(messages[0].content, "in-progress prompt");
}
