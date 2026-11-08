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
            finished: std::future::pending().boxed().shared(),
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

#[tokio::test]
async fn connection_cleanup_keeps_reservation_without_holding_global_lock_during_storage_wait() {
    let dir = tempfile::tempdir().unwrap();
    let key = SessionKey("local:blocked-storage-cleanup".into());
    let turn = TurnId::new();
    let sessions = Arc::new(TokioMutex::new(
        octos_bus::SessionManager::open(dir.path()).unwrap(),
    ));
    let storage_guard = sessions.clone().lock_owned().await;
    let child = tokio::spawn(std::future::pending::<()>());
    let child_abort = child.abort_handle();
    let child_finished = async move {
        let _ = child.await;
    }
    .boxed()
    .shared();
    let outer = tokio::spawn(std::future::pending::<()>());
    let mut entry = test_active_turn(turn.clone(), outer.abort_handle());
    entry.dispatched_prompt = Some(Arc::new(DispatchedTurnPrompt {
        text: "retain prompt".into(),
        sessions,
        thread_id: turn.0.to_string(),
        data_dir: dir.path().into(),
        context: Arc::new(StdMutex::new(ContextManager::new(key.to_string(), None))),
        agent_abort: child_abort,
        agent_finished: child_finished,
    }));
    let connection = ConnectionTurn {
        finished: std::future::pending().boxed().shared(),
        turn_id: turn,
        state: entry.state.clone(),
    };
    let active = Arc::new(TokioMutex::new(HashMap::from([(key.clone(), entry)])));
    let connections = Arc::new(TokioMutex::new(HashMap::from([(key.clone(), connection)])));
    let cleanup = {
        let active = active.clone();
        let connections = connections.clone();
        tokio::spawn(async move {
            abort_connection_turns(
                &active,
                &connections,
                &ScopePolicy::default(),
                &UiProtocolLedger::new(16),
                &PendingApprovalStore::default(),
                &PendingQuestionStore::default(),
            )
            .await;
        })
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        while !connections.lock().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // Cleanup has drained its connection and now waits on the held store.
    // A request for another session can still use the process-wide registry.
    let registry = tokio::time::timeout(Duration::from_millis(200), active.lock())
        .await
        .expect("blocked session storage must not freeze the global turn registry");
    assert!(
        registry.contains_key(&key),
        "keep this session reserved during cleanup"
    );
    drop(registry);
    assert!(!cleanup.is_finished());
    drop(storage_guard);
    tokio::time::timeout(Duration::from_secs(5), cleanup)
        .await
        .unwrap()
        .unwrap();
    assert!(active.lock().await.is_empty());
    assert!(outer.await.unwrap_err().is_cancelled());
}

struct D04TranscriptLlm {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl octos_llm::LlmProvider for D04TranscriptLlm {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> eyre::Result<octos_llm::ChatResponse> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(octos_llm::ChatResponse {
            content: (call > 0).then(|| "D04 FINAL RESPONSE".to_owned()),
            reasoning_content: None,
            tool_calls: if call == 0 {
                vec![octos_core::ToolCall {
                    id: "d04-tool-call".into(),
                    name: "definitely_missing_tool".into(),
                    arguments: json!({}),
                    metadata: None,
                }]
            } else {
                Vec::new()
            },
            stop_reason: if call == 0 {
                octos_llm::StopReason::ToolUse
            } else {
                octos_llm::StopReason::EndTurn
            },
            usage: octos_llm::TokenUsage::default(),
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "d04-transcript-stub"
    }

    fn provider_name(&self) -> &str {
        "stub"
    }
}

#[derive(Clone, Copy)]
enum D04Fault {
    None,
    AfterUser,
    AfterTool,
}

async fn d04_transcript_case(fault: D04Fault) -> (Vec<UiProtocolLedgerEvent>, Vec<Message>) {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(D04TranscriptLlm {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let (state, profile) = state_with_profile_llm(dir.path(), MAIN_PROFILE_ID, provider).await;
    let session = SessionKey::new("api", &format!("d04-transcript-{}", TurnId::new().0));
    let turn = TurnId::new();
    let _runtime = state
        .session_cache
        .get_or_init(&profile, session.clone(), None)
        .await
        .unwrap();
    // The canonical persist path writes the per-user layout
    // (`{data_dir}/users/{base}/sessions/{topic}.jsonl`); the Manager's flat
    // `session_path` is the legacy location and no longer receives commits.
    let session_path = profile
        .data_dir
        .join("users")
        .join(octos_bus::session::encode_path_component(
            session.base_key(),
        ))
        .join("sessions")
        .join(format!(
            "{}.jsonl",
            octos_bus::session::encode_path_component(session.topic().unwrap_or("default"))
        ));
    let backup_path = session_path.with_extension("jsonl.d04-backup");
    let ledger = Arc::new(UiProtocolLedger::new(128));
    let project = message_commit_observer(ledger.clone());
    let injected = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observer_injected = injected.clone();
    let observer_path = session_path.clone();
    let observer_backup = backup_path.clone();
    let observer: octos_bus::MessageCommitObserver = Arc::new(move |key, message, seq| {
        project(key, message, seq);
        let inject = match fault {
            D04Fault::None => false,
            D04Fault::AfterUser => message.role == MessageRole::User,
            D04Fault::AfterTool => message.role == MessageRole::Tool,
        };
        if inject && !observer_injected.swap(true, Ordering::SeqCst) {
            std::fs::rename(&observer_path, &observer_backup).unwrap();
            std::fs::create_dir(&observer_path).unwrap();
        }
    });
    octos_bus::session::set_scoped_message_commit_observer(&profile.data_dir, &observer);
    let contracts = Arc::new(UiProtocolContractStores::default());
    let active = Arc::new(TokioMutex::new(HashMap::new()));
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
        "start-d04".into(),
        TurnStartParams {
            session_id: session.clone(),
            turn_id: turn.clone(),
            input: vec![InputItem::Text {
                text: "produce a tool call and final response".into(),
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
    let accepted = recv_rpc_response_with_id(&mut rx, "start-d04").await;
    assert_eq!(accepted["result"]["accepted"], true, "{accepted}");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let frame = recv_rpc_json(&mut rx).await;
            let method = frame["method"].as_str();
            if matches!(method, Some("turn/completed" | "turn/error"))
                || frame["params"]["payload"]["type"] == "turn_terminal"
            {
                break;
            }
        }
    })
    .await
    .expect("turn must settle after transcript write failure");
    if injected.load(Ordering::SeqCst) {
        std::fs::remove_dir(&session_path).unwrap();
        std::fs::rename(&backup_path, &session_path).unwrap();
    } else {
        assert!(matches!(fault, D04Fault::None), "fault was not injected");
    }
    let rows = ledger
        .replay_after(
            &session,
            Some(&UiCursor {
                stream: session.to_string(),
                seq: 0,
            }),
        )
        .unwrap()
        .into_iter()
        .map(|row| row.event)
        .collect();
    let mut reopened = octos_bus::SessionManager::open(&profile.data_dir).unwrap();
    let history = reopened.get_or_create(&session).await.messages.clone();
    (rows, history)
}

fn d04_terminal_error(events: &[UiProtocolLedgerEvent]) -> Option<&TurnErrorEvent> {
    events.iter().find_map(|event| match event {
        UiProtocolLedgerEvent::Notification(UiNotification::TurnError(error)) => Some(error),
        _ => None,
    })
}

fn d04_completed(events: &[UiProtocolLedgerEvent]) -> bool {
    events.iter().any(|event| {
        matches!(
            event,
            UiProtocolLedgerEvent::Notification(UiNotification::TurnCompleted(_))
        )
    })
}

#[tokio::test(flavor = "current_thread")]
async fn should_fail_partial_turn_when_intermediate_transcript_commit_fails() {
    let (events, history) = d04_transcript_case(D04Fault::AfterUser).await;
    let error = d04_terminal_error(&events).unwrap_or_else(|| {
        panic!(
            "persistence_failed terminal missing; completed={}",
            d04_completed(&events)
        )
    });
    assert_eq!(error.code, "persistence_failed");
    assert!(
        error.message.contains("operation=transcript_message"),
        "{}",
        error.message
    );
    assert!(error.message.contains("session="), "{}", error.message);
    assert!(error.message.contains("turn="), "{}", error.message);
    assert!(error.partial_result.is_some());
    assert!(!d04_completed(&events));
    assert!(
        !history
            .iter()
            .any(|message| message.content == "D04 FINAL RESPONSE")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn should_fail_partial_turn_when_final_assistant_commit_fails() {
    let (events, history) = d04_transcript_case(D04Fault::AfterTool).await;
    let error = d04_terminal_error(&events).unwrap_or_else(|| {
        panic!(
            "persistence_failed terminal missing; completed={}",
            d04_completed(&events)
        )
    });
    assert_eq!(error.code, "persistence_failed");
    assert!(
        error.message.contains("operation=final_assistant"),
        "{}",
        error.message
    );
    assert!(error.partial_result.is_some());
    assert!(!d04_completed(&events));
    assert!(
        !history
            .iter()
            .any(|message| message.content == "D04 FINAL RESPONSE")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn should_complete_and_reopen_normally_when_transcript_commits_succeed() {
    let (events, history) = d04_transcript_case(D04Fault::None).await;
    assert!(d04_completed(&events));
    assert!(d04_terminal_error(&events).is_none());
    assert!(
        history
            .iter()
            .any(|message| message.content == "D04 FINAL RESPONSE")
    );
    assert!(
        history
            .iter()
            .any(|message| message.tool_call_id.as_deref() == Some("d04-tool-call"))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn should_fail_review_when_final_summary_cannot_be_persisted() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(AppuiContinuationLlm::new("D04 REVIEW SUMMARY"));
    let (state, profile) = state_with_profile_llm(dir.path(), MAIN_PROFILE_ID, provider).await;
    let session = SessionKey::new("api", &format!("d04-review-{}", TurnId::new().0));
    let turn = TurnId::new();
    let _runtime = state
        .session_cache
        .get_or_init(&profile, session.clone(), None)
        .await
        .unwrap();
    let session_path = profile
        .data_dir
        .join("users")
        .join(octos_bus::session::encode_path_component(
            session.base_key(),
        ))
        .join("sessions")
        .join(format!(
            "{}.jsonl",
            octos_bus::session::encode_path_component(session.topic().unwrap_or("default"))
        ));
    std::fs::create_dir_all(session_path.parent().unwrap()).unwrap();
    std::fs::create_dir(&session_path).unwrap();
    let ledger = Arc::new(UiProtocolLedger::new(128));
    let observer = message_commit_observer(ledger.clone());
    octos_bus::session::set_scoped_message_commit_observer(&profile.data_dir, &observer);
    let contracts = Arc::new(UiProtocolContractStores::default());
    let turn_state = Arc::new(TokioMutex::new(TurnState::Active));
    let (ws, mut rx) = ws_connection_for_test(128);
    let (_interrupt_tx, interrupt_rx) = mpsc::channel(1);
    run_native_code_review_turn(
        ws,
        state,
        ledger.clone(),
        contracts,
        ConnectionUiFeatures::stdio_defaults(),
        RawReviewStartParams {
            session_id: session.clone(),
            profile_id: None,
            turn_id: Some(turn.clone()),
            target: None,
            prompt: Some("review the D-04 persistence boundary".into()),
            instructions: None,
            delivery: None,
        },
        turn,
        None,
        turn_state,
        interrupt_rx,
    )
    .await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let frame = recv_rpc_json(&mut rx).await;
            let method = frame["method"].as_str();
            if matches!(method, Some("turn/completed" | "turn/error"))
                || frame["params"]["payload"]["type"] == "turn_terminal"
            {
                break;
            }
        }
    })
    .await
    .expect("review must settle after summary write failure");
    let rows = ledger
        .replay_after(
            &session,
            Some(&UiCursor {
                stream: session.to_string(),
                seq: 0,
            }),
        )
        .unwrap()
        .into_iter()
        .map(|row| row.event)
        .collect::<Vec<_>>();
    let error = d04_terminal_error(&rows).unwrap_or_else(|| {
        panic!(
            "review persistence_failed terminal missing; completed={}",
            d04_completed(&rows)
        )
    });
    assert_eq!(error.code, "persistence_failed");
    assert!(
        error.message.contains("operation=review_summary"),
        "{}",
        error.message
    );
    assert!(error.partial_result.is_some());
    assert!(!d04_completed(&rows));
    assert!(rows.iter().any(|event| matches!(
        event,
        UiProtocolLedgerEvent::Notification(UiNotification::TaskUpdated(task))
            if task.state == UiTaskRuntimeState::Failed
    )));
    std::fs::remove_dir(&session_path).unwrap();
    let history = octos_bus::session::load_session_messages_locked(&profile.data_dir, &session)
        .await
        .unwrap_or_default();
    assert!(
        !history
            .iter()
            .any(|message| message.content.contains("D04 REVIEW SUMMARY"))
    );
}
