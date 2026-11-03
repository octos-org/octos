//! Cancellation must be durable, idempotent and scoped like the request.
use super::*;

fn cancellations(
    ledger: &UiProtocolLedger,
    session: &SessionKey,
) -> Vec<UserQuestionCancelledEvent> {
    ledger
        .replay_after(session, Some(&r2625_cursor_zero(session)))
        .unwrap()
        .into_iter()
        .filter_map(|row| match row.event {
            UiProtocolLedgerEvent::Notification(UiNotification::UserQuestionCancelled(e)) => {
                Some(e)
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn connection_abort_persists_question_cancel_once_and_rejects_late_answer() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(dir.path().to_owned()));
    let session = SessionKey("local:cancel#coding".into());
    let turn = TurnId::new();
    let question = sample_pending_question(session.clone(), QuestionId::new(), turn.clone());
    let contracts = UiProtocolContractStores::default();
    let waiter = contracts.user_questions.request_runtime(question.clone());
    ledger.append_notification(UiNotification::UserQuestionRequested(question.clone()));
    let active: SharedActiveTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let connections: SharedConnectionTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let handle = tokio::spawn(std::future::pending::<()>());
    active.lock().await.insert(
        session.clone(),
        test_active_turn(turn.clone(), handle.abort_handle()),
    );
    connections.lock().await.insert(
        session.clone(),
        test_connection_turn(&active, &session, &turn).await,
    );
    // Real server-owned detach keeps both the task and pending question alive.
    let registered = connections.lock().await.clone();
    detach_connection_turns(&active, &connections, &contracts.scopes).await;
    assert_eq!(
        contracts.user_questions.pending_for_session(&session).len(),
        1
    );
    assert!(cancellations(&ledger, &session).is_empty());
    *connections.lock().await = registered;
    for _ in 0..2 {
        abort_connection_turns(
            &active,
            &connections,
            &contracts.scopes,
            &ledger,
            &contracts.approvals,
            &contracts.user_questions,
        )
        .await;
    }
    handle.abort();
    let _ = handle.await;
    assert!(waiter.await.is_err());
    let cancelled = cancellations(&ledger, &session);
    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0].question_id, question.question_id);
    assert_eq!(cancelled[0].turn_id, turn);
    assert_eq!(cancelled[0].topic.as_deref(), Some("coding"));
    assert_eq!(cancelled[0].reason, "turn_interrupted");
    let late = contracts
        .user_questions
        .respond_with_context(&UserQuestionRespondParams::new(
            session.clone(),
            question.question_id,
            g1_answer(),
        ))
        .unwrap_err();
    assert_eq!(late.code, rpc_error_codes::USER_QUESTION_STALE);
    assert!(
        contracts
            .user_questions
            .pending_for_session(&session)
            .is_empty()
    );
    drop(ledger);
    let recovered = UiProtocolLedger::with_config(LedgerConfig::durable(dir.path().to_owned()));
    assert_eq!(cancellations(&recovered, &session), cancelled);
}

#[tokio::test]
async fn waiter_drop_broadcasts_once_and_does_not_cancel_an_answered_question() {
    let ledger = Arc::new(UiProtocolLedger::new(32));
    let contracts = Arc::new(UiProtocolContractStores::default());
    let session = SessionKey("local:guard".into());
    let mut live = ledger.subscribe(&session);
    for answered in [false, true] {
        let question = sample_pending_question(session.clone(), QuestionId::new(), TurnId::new());
        let waiter = contracts.user_questions.request_runtime(question.clone());
        let guard = || {
            PendingQuestionWaiterGuard::new(
                contracts.clone(),
                ledger.clone(),
                session.clone(),
                question.question_id.clone(),
            )
        };
        if answered {
            contracts
                .user_questions
                .respond_with_context(&UserQuestionRespondParams::new(
                    session.clone(),
                    question.question_id.clone(),
                    g1_answer(),
                ))
                .unwrap();
        }
        let first = guard();
        let competing = guard();
        drop(first);
        drop(competing);
        assert_eq!(waiter.await.is_ok(), answered);
    }
    assert_eq!(cancellations(&ledger, &session).len(), 1);
    let event = live.try_recv().unwrap();
    let UiProtocolLedgerEvent::Notification(UiNotification::UserQuestionCancelled(e)) = event.event
    else {
        panic!("wrong event")
    };
    assert_eq!(e.reason, "waiter_dropped");
    assert!(live.try_recv().is_err());
}

#[tokio::test]
async fn failed_external_question_send_records_requested_then_cancelled() {
    use octos_agent::UserQuestionRequester as _;
    let dir = tempfile::tempdir().unwrap();
    let session = g1_system_session();
    let state = g1_state(dir.path(), &session).await;
    let ledger = Arc::new(UiProtocolLedger::new(32));
    let contracts = Arc::new(UiProtocolContractStores::default());
    let (ws, rx) = ws_connection_for_test(8);
    ws.update_live_features(features_with_user_question_v1());
    ws.set_external(true);
    ws.mark_failed();
    drop(rx);
    let requester = SessionUserQuestionRequester {
        ws,
        ledger: ledger.clone(),
        contracts: contracts.clone(),
        state,
        peers_root: dir.path().join("peers"),
        session_id: session.clone(),
        turn_id: TurnId::new(),
    };
    let result = requester
        .request_user_question(octos_agent::UserQuestionRequest {
            title: "Choose".into(),
            body: "Choose".into(),
            questions: sample_pending_question(session.clone(), QuestionId::new(), TurnId::new())
                .questions,
        })
        .await;
    assert!(matches!(
        result,
        octos_agent::UserQuestionOutcome::Unsupported
    ));
    let events = ledger
        .replay_after(&session, Some(&r2625_cursor_zero(&session)))
        .unwrap();
    assert_eq!(events.len(), 2);
    assert!(matches!(
        events[0].event,
        UiProtocolLedgerEvent::Notification(UiNotification::UserQuestionRequested(_))
    ));
    assert!(events.iter().all(|e| e.external_prompt));
    assert_eq!(
        cancellations(&ledger, &session)[0].reason,
        "request_send_failed"
    );
    assert!(
        contracts
            .user_questions
            .pending_for_session(&session)
            .is_empty()
    );
}

#[tokio::test]
async fn cancellation_capability_gate_covers_direct_live_and_open_replay() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with_sessions(dir.path());
    let ledger = Arc::new(UiProtocolLedger::new(16));
    let session = SessionKey("local:gate".into());
    let event =
        UiNotification::UserQuestionCancelled(UserQuestionCancelledEvent::turn_interrupted(
            session.clone(),
            QuestionId::new(),
            TurnId::new(),
        ));
    let (ws, mut rx) = ws_connection_for_test(8);
    ws.update_live_features(ConnectionUiFeatures::default());
    send_notification_durable(&ws, &ledger, event).unwrap();
    assert!(rx.try_recv().is_err());
    let row = ledger
        .replay_after(&session, Some(&r2625_cursor_zero(&session)))
        .unwrap()
        .pop()
        .unwrap();
    for enabled in [false, true] {
        let features = ConnectionUiFeatures {
            user_question_v1: enabled,
            ..Default::default()
        };
        let (reconnect, mut frames) = ws_connection_for_test(64);
        reconnect.update_live_features(features);
        let forwarders: SharedLiveForwarders = Arc::new(TokioMutex::new(HashMap::new()));
        assert!(
            handle_session_open(
                &reconnect,
                &state,
                &ledger,
                &PendingApprovalStore::default(),
                &PendingQuestionStore::default(),
                &forwarders,
                None,
                None,
                features,
                "open".into(),
                SessionOpenParams {
                    session_id: session.clone(),
                    session_ref: None,
                    topic: None,
                    profile_id: None,
                    cwd: None,
                    sandbox: None,
                    after: Some(r2625_cursor_zero(&session)),
                    client_commands: None
                },
                false
            )
            .await
        );
        let mut replayed = false;
        while let Ok(frame) = frames.try_recv() {
            let WsMessage::Text(text) = frame else {
                continue;
            };
            let value: Value = serde_json::from_str(text.as_str()).unwrap();
            replayed |= value["method"] == "user_question/cancelled";
        }
        assert_eq!(replayed, enabled);
        abort_live_forwarders(&forwarders, &ledger).await;
        // Exercise the actual live delivery boundary with the same event.
        forward_live_ledger_event(
            &reconnect,
            &ledger,
            row.clone(),
            0,
            reconnect.connection_id,
            features,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(frames.try_recv().is_ok(), enabled);
    }
}

#[tokio::test]
async fn external_cancellation_stays_private_after_owner_eviction_and_cold_reload() {
    use super::super::super::host_managed::{forget_external_prompt, register_external_prompt};
    let dir = tempfile::tempdir().unwrap();
    let session = g1_system_session();
    let state = g1_state(dir.path(), &session).await;
    let ledger_dir = dir.path().join("ledger");
    let ledger = UiProtocolLedger::with_config(LedgerConfig::durable(ledger_dir.clone()));
    let contracts = UiProtocolContractStores::default();
    let (host, mut host_rx) = ws_connection_for_test(16);
    let (owner, _owner_rx) = ws_connection_for_test(16);
    host.update_live_features(features_with_user_question_v1());
    let q = sample_pending_question(session.clone(), QuestionId::new(), TurnId::new());
    let id = q.question_id.0.to_string();
    register_external_prompt(&id, owner.connection_id.0);
    let waiter = contracts
        .user_questions
        .request_runtime_owned(q.clone(), Some(owner.connection_id.0));
    contracts.user_questions.mark_external(&q.question_id);
    ledger.append_notification(UiNotification::UserQuestionRequested(q.clone()));
    // The request is persisted before the bounded owner table forgets it.
    forget_external_prompt(&id);
    let event = cancel_question_notification(
        &contracts.user_questions,
        &session,
        &q.question_id,
        "peer_closed",
    )
    .unwrap();
    send_notification_durable(&host, &ledger, event).unwrap();
    assert!(
        host_rx.try_recv().is_err(),
        "host must not see external cancellation directly"
    );
    assert!(waiter.await.is_err());
    let row = ledger
        .replay_after(&session, Some(&r2625_cursor_zero(&session)))
        .unwrap()
        .pop()
        .unwrap();
    assert!(row.external_prompt);
    assert!(!ledgered_event_visible_to_connection(
        &row,
        host.connection_id
    ));
    assert!(replayed_event_visible_to_connection(
        &row,
        owner.connection_id,
        &contracts.approvals,
        &contracts.user_questions
    ));
    assert!(!replayed_event_visible_to_connection(
        &row,
        host.connection_id,
        &contracts.approvals,
        &contracts.user_questions
    ));
    let ids =
        r2625_opened_prompt_ids(&state, &ledger, &contracts, owner.connection_id, &session).await;
    assert_eq!(ids.iter().filter(|found| *found == &id).count(), 2);
    assert!(
        r2625_opened_prompt_ids(&state, &ledger, &contracts, host.connection_id, &session)
            .await
            .is_empty()
    );
    // If the owner is still known, live delivery admits only that owner.
    register_external_prompt(&id, owner.connection_id.0);
    assert!(ledgered_event_visible_to_connection(
        &row,
        owner.connection_id
    ));
    assert!(!ledgered_event_visible_to_connection(
        &row,
        host.connection_id
    ));
    forget_external_prompt(&id);
    drop(ledger);
    let recovered = UiProtocolLedger::with_config(LedgerConfig::durable(ledger_dir));
    let empty = UiProtocolContractStores::default();
    for connection in [owner.connection_id, host.connection_id] {
        assert!(
            r2625_opened_prompt_ids(&state, &recovered, &empty, connection, &session)
                .await
                .is_empty()
        );
    }
    assert_eq!(cancellations(&recovered, &session).len(), 1);
}

#[test]
fn peer_close_and_post_registration_race_each_emit_one_question_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let peers = dir.path().join("peers");
    let master = SessionKey::with_profile_topic("dev", "local", "tui", "coding");
    let (slug, session) = stage_and_open_peer(&peers, "question-close", "close-question", &master);
    let contracts = UiProtocolContractStores::default();
    let ledger = UiProtocolLedger::new(32);
    let q = sample_pending_question(session.clone(), QuestionId::new(), TurnId::new());
    let _waiter = contracts.user_questions.request_runtime(q);
    for _ in 0..2 {
        cancel_peer_pending_on_close(&contracts, "question-close", &slug, &|event| {
            ledger.append_notification(event);
        });
    }
    std::fs::write(peers.join(&slug).join("closed"), "closed").unwrap();
    let late = sample_pending_question(session.clone(), QuestionId::new(), TurnId::new());
    let _late_waiter = contracts.user_questions.request_runtime(late.clone());
    let gate = PeerParkGate::for_session(&peers, &session);
    for _ in 0..2 {
        assert!(refuse_registered_question_park_for_closed_peer(
            gate.as_ref(),
            &contracts,
            &ledger,
            &session,
            &late.question_id
        ));
    }
    let events = cancellations(&ledger, &session);
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|event| event.reason == "peer_closed"));
}

/// #1463 — an interrupted M9 fixture turn must drain pending user questions
/// exactly like the live interrupt path (and like approvals on the same
/// branch): the runtime waiter closes (Cancelled), a late respond is stale
/// with `turn_interrupted`, and reconnect hydration no longer re-shows the
/// dead turn's question.
#[tokio::test]
async fn m9_fixture_interrupt_cancels_pending_user_questions() {
    let (ws, _rx) = ws_connection_for_test(32);
    let state = Arc::new(AppState::empty_for_tests());
    let ledger = Arc::new(UiProtocolLedger::new(32));
    let contracts = Arc::new(UiProtocolContractStores::default());
    let session_id = SessionKey("local:test".into());
    let turn_id = TurnId::new();
    let surviving_turn = TurnId::new();
    let question_id = QuestionId::new();
    let surviving_question_id = QuestionId::new();

    let framework_question = || {
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
        }]
    };
    let waiter_rx = contracts
        .user_questions
        .request_runtime(UserQuestionRequestedEvent::new(
            session_id.clone(),
            question_id.clone(),
            turn_id.clone(),
            "Pick a framework",
            "Which framework should I scaffold?",
            framework_question(),
        ));
    contracts
        .user_questions
        .request_runtime(UserQuestionRequestedEvent::new(
            session_id.clone(),
            surviving_question_id.clone(),
            surviving_turn,
            "Pick a framework",
            "Which framework should I scaffold?",
            framework_question(),
        ));
    assert_eq!(
        contracts
            .user_questions
            .pending_for_session(&session_id)
            .len(),
        2
    );

    let params = TurnStartParams {
        session_id: session_id.clone(),
        turn_id: turn_id.clone(),
        input: vec![InputItem::Text {
            text: "m9 slow fixture".into(),
        }],
        media: Vec::new(),
        topic: None,
        rewrite_for: None,
        reasoning_effort: None,
        tool_context: None,
        live_video: false,
        origin: None,
    };
    let turn_state = Arc::new(TokioMutex::new(TurnState::Active));
    let (interrupt_tx, interrupt_rx) = mpsc::channel::<()>(1);
    interrupt_tx
        .try_send(())
        .expect("preload pending interrupt");
    drop(interrupt_tx);

    run_m9_fixture_turn(
        ws,
        state,
        ledger.clone(),
        Arc::clone(&contracts),
        params,
        M9ProtocolFixture::Slow,
        turn_state,
        interrupt_rx,
    )
    .await;

    let cancelled = ledger
        .replay_after(&session_id, Some(&r2625_cursor_zero(&session_id)))
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.event {
            UiProtocolLedgerEvent::Notification(UiNotification::UserQuestionCancelled(e)) => {
                Some(e)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0].question_id, question_id);
    assert_eq!(cancelled[0].reason, "turn_interrupted");

    // The blocked tool's waiter closed (Cancelled). Bounded wait: without the
    // fix the sender lives on in the store and this receiver never resolves.
    let waiter_closed = tokio::time::timeout(std::time::Duration::from_secs(5), waiter_rx)
        .await
        .expect("interrupt must close the pending question's runtime waiter");
    assert!(
        waiter_closed.is_err(),
        "a cancelled question's waiter resolves to Cancelled, not answers"
    );
    // …a late respond is stale with the precise reason…
    let answer = || {
        vec![UserQuestionAnswer {
            selected_labels: vec!["axum".into()],
            free_text: None,
        }]
    };
    let err = contracts
        .user_questions
        .respond_with_context(&UserQuestionRespondParams::new(
            session_id.clone(),
            question_id,
            answer(),
        ))
        .expect_err("late respond against interrupted-turn question");
    assert_eq!(err.code, rpc_error_codes::USER_QUESTION_STALE);
    assert_eq!(
        err.data.as_ref().unwrap()["reason"],
        json!("turn_interrupted")
    );
    // …reconnect hydration only re-shows the surviving turn's question…
    let pending = contracts.user_questions.pending_for_session(&session_id);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].question_id, surviving_question_id.clone());
    // …which stays answerable.
    let ok = contracts
        .user_questions
        .respond_with_context(&UserQuestionRespondParams::new(
            session_id,
            surviving_question_id,
            answer(),
        ))
        .expect("non-interrupted turn question still pending");
    assert!(ok.result.accepted);
}

#[tokio::test]
async fn interrupted_batch_is_durable_before_a_blocked_send_can_be_aborted() {
    let session = SessionKey("local:blocked-cancellation".into());
    let turn = TurnId::new();
    let contracts = UiProtocolContractStores::default();
    let ledger = UiProtocolLedger::new(16);
    let (tx, _rx) = std::sync::mpsc::sync_channel(1);
    tx.send(WsMessage::Text("occupied".into())).unwrap();
    let ws = WsConnection::new_stdio(tx);
    ws.update_live_features(features_with_user_question_v1());
    let mut waiters = Vec::new();
    for _ in 0..3 {
        waiters.push(
            contracts
                .user_questions
                .request_runtime(sample_pending_question(
                    session.clone(),
                    QuestionId::new(),
                    turn.clone(),
                )),
        );
    }
    assert!(
        tokio::time::timeout(
            Duration::from_millis(30),
            cancel_turn_questions_durable(&ws, &ledger, &contracts.user_questions, &session, &turn,)
        )
        .await
        .is_err(),
        "the full stdio queue must leave delivery blocked"
    );
    assert_eq!(cancellations(&ledger, &session).len(), 3);
    for waiter in waiters {
        assert!(waiter.await.is_err());
    }
    // Retry after dropping the delivery future: no extra events, no queue wait.
    cancel_turn_questions_durable(&ws, &ledger, &contracts.user_questions, &session, &turn).await;
    assert_eq!(cancellations(&ledger, &session).len(), 3);
}
