#[test]
fn turn_terminal_projection_preserves_time_across_repeats_and_cold_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let config = LedgerConfig::durable(temp.path().join("terminal-time-ledger"));
    let ledger = UiProtocolLedger::with_config(config.clone());
    let session_id = SessionKey("local:terminal-time".into());
    let completed = TurnId::new();
    let interrupted = TurnId::new();
    let active = TurnId::new();
    for turn in [&completed, &interrupted, &active] {
        ledger.append_notification(UiNotification::TurnStarted(
            octos_core::ui_protocol::TurnStartedEvent {
                session_id: session_id.clone(),
                turn_id: turn.clone(),
                timestamp: Utc::now(),
                topic: None,
            },
        ));
    }
    let completed_terminal =
        ledger.append_notification(UiNotification::TurnCompleted(TurnCompletedEvent {
            session_id: session_id.clone(),
            topic: None,
            turn_id: completed.clone(),
            cursor: None,
            tokens_in: None,
            tokens_out: None,
            session_result: None,
            token_usage: None,
        }));
    let interrupted_terminal =
        ledger.append_notification(UiNotification::TurnError(TurnErrorEvent {
            session_id: session_id.clone(),
            topic: None,
            turn_id: interrupted.clone(),
            code: "interrupted".into(),
            message: "stop".into(),
            token_usage: None,
            partial_result: None,
        }));

    let (events, _) = ledger.snapshot_with_cursor(&session_id, None).unwrap();
    let initial = project_turn_from_ledger(&completed, &events);
    let initial_interrupted = project_turn_from_ledger(&interrupted, &events);
    let initial_active = project_turn_from_ledger(&active, &events);
    assert_eq!(initial.state, Some(TurnLifecycleState::Completed));
    assert_eq!(
        initial_interrupted.state,
        Some(TurnLifecycleState::Interrupted)
    );
    assert_eq!(initial.completed_at, completed_terminal.recorded_at);
    assert_eq!(
        initial_interrupted.completed_at,
        interrupted_terminal.recorded_at
    );
    assert_eq!(initial_active.completed_at, None);
    assert_eq!(initial.completed_at_durable, Some(true));
    assert_eq!(initial_interrupted.completed_at_durable, Some(true));
    assert_eq!(initial_active.completed_at_durable, None);

    std::thread::sleep(std::time::Duration::from_millis(2));
    assert_eq!(
        project_turn_from_ledger(&completed, &events).completed_at,
        initial.completed_at,
        "reprojection must not advance terminal time"
    );
    assert_eq!(
        project_turn_from_ledger(&interrupted, &events).completed_at,
        initial_interrupted.completed_at
    );

    drop(ledger);
    let recovered = UiProtocolLedger::recover(config).ledger;
    let (cold_events, _) = recovered
        .snapshot_with_cursor(&session_id, None)
        .expect("cold terminal-time snapshot");
    assert_eq!(
        project_turn_from_ledger(&completed, &cold_events).completed_at,
        initial.completed_at,
        "cold recovery must preserve the durable terminal timestamp"
    );
    assert_eq!(
        project_turn_from_ledger(&interrupted, &cold_events).completed_at,
        initial_interrupted.completed_at
    );
    assert_eq!(
        project_turn_from_ledger(&completed, &cold_events).completed_at_durable,
        Some(true)
    );
    assert_eq!(
        project_turn_from_ledger(&interrupted, &cold_events).completed_at_durable,
        Some(true)
    );
}

#[test]
fn turn_terminal_projection_leaves_legacy_time_absent() {
    let session_id = SessionKey("local:legacy-terminal-time".into());
    let turn_id = TurnId::new();
    let events = vec![LedgeredUiProtocolEvent {
        cursor: UiCursor {
            stream: session_id.0.clone(),
            seq: 1,
        },
        recorded_at: None,
        event: UiProtocolLedgerEvent::Notification(UiNotification::TurnCompleted(
            TurnCompletedEvent {
                session_id,
                topic: None,
                turn_id: turn_id.clone(),
                cursor: None,
                tokens_in: None,
                tokens_out: None,
                session_result: None,
                token_usage: None,
            },
        )),
        from_connection: None,
        external_prompt: false,
    }];
    let projection = project_turn_from_ledger(&turn_id, &events);
    assert_eq!(projection.state, Some(TurnLifecycleState::Completed));
    assert_eq!(
        projection.completed_at, None,
        "legacy terminal events must not acquire a synthetic snapshot timestamp"
    );
    assert_eq!(projection.completed_at_durable, None);
}

#[test]
fn turn_terminal_projection_marks_restart_recovery_not_durable() {
    let temp = tempfile::tempdir().unwrap();
    let config = LedgerConfig::durable(temp.path().join("restart-terminal-time"));
    let session_id = SessionKey("local:restart-terminal-time".into());
    let turn_id = TurnId::new();
    {
        let ledger = UiProtocolLedger::with_config(config.clone());
        ledger.append_notification(UiNotification::TurnStarted(
            octos_core::ui_protocol::TurnStartedEvent {
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
                timestamp: Utc::now(),
                topic: None,
            },
        ));
        // Simulates kill -9: the process dies without a terminal event.
    }

    let recovered = UiProtocolLedger::recover(config.clone()).ledger;
    let (events, _) = recovered
        .snapshot_with_cursor(&session_id, None)
        .expect("first recovery after old-core/process death");
    let synthetic = events
        .iter()
        .find(|event| matches!(
            &event.event,
            UiProtocolLedgerEvent::Notification(UiNotification::TurnError(error))
                if error.code == "orphaned_by_restart"
        ))
        .expect("restart must synthesize an orphan terminal");
    let projection = project_turn_from_ledger(&turn_id, &events);
    assert_eq!(projection.state, Some(TurnLifecycleState::Errored));
    assert_eq!(projection.completed_at, synthetic.recorded_at);
    assert_eq!(projection.completed_at_durable, None);
    let event_count = events.len();
    drop(recovered);

    let recovered_again = UiProtocolLedger::recover(config).ledger;
    let (events_again, _) = recovered_again
        .snapshot_with_cursor(&session_id, None)
        .expect("repeated cold recovery");
    assert_eq!(events_again.len(), event_count);
    let projected_again = project_turn_from_ledger(&turn_id, &events_again);
    assert_eq!(projected_again.completed_at, projection.completed_at);
    assert_eq!(projected_again.completed_at_durable, None);
}

#[test]
fn turn_terminal_recovery_orphans_reused_turn_generation() {
    for old_terminal in ["completed", "interrupted"] {
        let temp = tempfile::tempdir().unwrap();
        let config = LedgerConfig::durable(temp.path().join("reused-turn-recovery"));
        let session_id = SessionKey(format!("local:reused-turn-{old_terminal}"));
        let turn_id = TurnId::new();
        let reused_started_at = Utc::now();
        let started_at = reused_started_at - chrono::Duration::seconds(1);
        {
            let ledger = UiProtocolLedger::with_config(config.clone());
            ledger.append_notification(UiNotification::TurnStarted(
                octos_core::ui_protocol::TurnStartedEvent {
                    session_id: session_id.clone(),
                    turn_id: turn_id.clone(),
                    timestamp: started_at,
                    topic: None,
                },
            ));
            if old_terminal == "completed" {
                ledger.append_notification(UiNotification::TurnCompleted(TurnCompletedEvent {
                    session_id: session_id.clone(),
                    topic: None,
                    turn_id: turn_id.clone(),
                    cursor: None,
                    tokens_in: None,
                    tokens_out: None,
                    session_result: None,
                    token_usage: None,
                }));
            } else {
                ledger.append_notification(UiNotification::TurnError(TurnErrorEvent {
                    session_id: session_id.clone(),
                    topic: None,
                    turn_id: turn_id.clone(),
                    code: "interrupted".into(),
                    message: "old generation interrupted".into(),
                    token_usage: None,
                    partial_result: None,
                }));
            }
            ledger.append_notification(UiNotification::TurnStarted(
                octos_core::ui_protocol::TurnStartedEvent {
                    session_id: session_id.clone(),
                    turn_id: turn_id.clone(),
                    timestamp: reused_started_at,
                    topic: None,
                },
            ));
            // Simulates kill -9 after reusing the id without a new terminal.
        }

        let recovered = UiProtocolLedger::recover(config.clone()).ledger;
        let (events, _) = recovered
            .snapshot_with_cursor(&session_id, None)
            .expect("first recovery after reusing a terminal turn id");
        let synthetic: Vec<_> = events
            .iter()
            .filter(|event| matches!(
                &event.event,
                UiProtocolLedgerEvent::Notification(UiNotification::TurnError(error))
                    if error.code == "orphaned_by_restart"
            ))
            .collect();
        assert_eq!(
            synthetic.len(),
            1,
            "{old_terminal}: the new generation must not inherit the old terminal"
        );
        let projection = project_turn_from_ledger(&turn_id, &events);
        assert_eq!(projection.state, Some(TurnLifecycleState::Errored));
        assert_eq!(projection.started_at, Some(reused_started_at));
        assert_eq!(projection.completed_at, synthetic[0].recorded_at);
        assert!(projection.completed_at >= Some(reused_started_at));
        assert_eq!(projection.completed_at_durable, None);
        let event_count = events.len();
        drop(recovered);

        let recovered_again = UiProtocolLedger::recover(config).ledger;
        let (events_again, _) = recovered_again
            .snapshot_with_cursor(&session_id, None)
            .expect("second recovery is idempotent");
        assert_eq!(events_again.len(), event_count);
        let projected_again = project_turn_from_ledger(&turn_id, &events_again);
        assert_eq!(projected_again.state, Some(TurnLifecycleState::Errored));
        assert_eq!(projected_again.completed_at, projection.completed_at);
        assert_eq!(projected_again.completed_at_durable, None);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn reused_active_turn_clears_terminal_time_in_hydrate_and_state_get() {
    let session_id = SessionKey("local:reused-active-terminal-time".into());
    let state = prg_state_with_session(&session_id, |_| {});
    let active_turns = active_turns_registry();
    let ledger = event_ledger(&state).await;
    let turn_id = TurnId::new();
    ledger.append_notification(UiNotification::TurnStarted(
        octos_core::ui_protocol::TurnStartedEvent {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            timestamp: Utc::now(),
            topic: None,
        },
    ));
    ledger.append_notification(UiNotification::TurnCompleted(TurnCompletedEvent {
        session_id: session_id.clone(),
        topic: None,
        turn_id: turn_id.clone(),
        cursor: None,
        tokens_in: None,
        tokens_out: None,
        session_result: None,
        token_usage: None,
    }));

    let (ws, mut rx) = ws_connection_for_test(8);
    handle_turn_state_get(
        &ws,
        &state,
        &ledger,
        &active_turns,
        None,
        None,
        ConnectionUiFeatures::stdio_defaults(),
        "terminal-before-reuse".into(),
        TurnStateGetParams {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
        },
    )
    .await;
    let terminal = recv_rpc_json(&mut rx).await;
    assert_eq!(terminal["result"]["state"], "completed");
    assert_eq!(terminal["result"]["completed_at_durable"], true);
    let (events, _) = ledger.snapshot_with_cursor(&session_id, None).unwrap();
    let terminal_turns = collect_session_turns(&session_id, &active_turns, &events, &[]).await;
    assert_eq!(terminal_turns.len(), 1);
    assert_eq!(terminal_turns[0].completed_at_durable, Some(true));
    assert_eq!(
        serde_json::to_value(&terminal_turns[0]).unwrap()["completed_at_durable"],
        true
    );

    let handle = tokio::spawn(async {});
    active_turns.lock().await.insert(
        session_id.clone(),
        test_active_turn(turn_id.clone(), handle.abort_handle()),
    );
    handle_turn_state_get(
        &ws,
        &state,
        &ledger,
        &active_turns,
        None,
        None,
        ConnectionUiFeatures::stdio_defaults(),
        "active-after-reuse".into(),
        TurnStateGetParams {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
        },
    )
    .await;
    let active = recv_rpc_json(&mut rx).await;
    assert_eq!(active["result"]["state"], "active");
    assert!(active["result"].get("completed_at").is_none());
    assert!(active["result"].get("completed_at_durable").is_none());

    let turn_state = active_turns
        .lock()
        .await
        .get(&session_id)
        .unwrap()
        .state
        .clone();
    let (ack, _wait_ack) = tokio::sync::oneshot::channel();
    *turn_state.lock().await = TurnState::Interrupting {
        ack,
        origin: InterruptOrigin::Client,
    };
    handle_turn_state_get(
        &ws,
        &state,
        &ledger,
        &active_turns,
        None,
        None,
        ConnectionUiFeatures::stdio_defaults(),
        "interrupting-after-reuse".into(),
        TurnStateGetParams {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
        },
    )
    .await;
    let interrupting = recv_rpc_json(&mut rx).await;
    assert_eq!(interrupting["result"]["state"], "interrupting");
    assert!(interrupting["result"].get("completed_at").is_none());
    assert!(interrupting["result"]
        .get("completed_at_durable")
        .is_none());

    let turns = collect_session_turns(&session_id, &active_turns, &events, &[]).await;
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].state, TurnLifecycleState::Interrupting);
    assert_eq!(turns[0].completed_at, None);
    assert_eq!(turns[0].completed_at_durable, None);
    let serialized = serde_json::to_value(&turns[0]).unwrap();
    assert!(serialized.get("completed_at_durable").is_none());
    active_turns.lock().await.remove(&session_id);
    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn restart_recovery_shape_is_not_durable_in_hydrate_or_state_get() {
    let session_id = SessionKey("local:restart-shape-terminal-time".into());
    let state = prg_state_with_session(&session_id, |_| {});
    let active_turns = active_turns_registry();
    let ledger = event_ledger(&state).await;
    let turn_id = TurnId::new();
    ledger.append_notification(UiNotification::TurnStarted(
        octos_core::ui_protocol::TurnStartedEvent {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            timestamp: Utc::now(),
            topic: None,
        },
    ));
    let terminal = ledger.append_notification(UiNotification::TurnError(TurnErrorEvent {
        session_id: session_id.clone(),
        topic: None,
        turn_id: turn_id.clone(),
        code: "orphaned_by_restart".into(),
        message: "server restarted before this turn finished".into(),
        token_usage: None,
        partial_result: None,
    }));
    let (ws, mut rx) = ws_connection_for_test(8);
    handle_turn_state_get(
        &ws,
        &state,
        &ledger,
        &active_turns,
        None,
        None,
        ConnectionUiFeatures::stdio_defaults(),
        "restart-shape".into(),
        TurnStateGetParams {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["result"]["state"], "errored");
    assert_eq!(
        frame["result"]["completed_at"]
            .as_str()
            .unwrap()
            .parse::<chrono::DateTime<Utc>>()
            .unwrap(),
        terminal.recorded_at.unwrap()
    );
    assert!(frame["result"].get("completed_at_durable").is_none());

    let real_turn_id = TurnId::new();
    ledger.append_notification(UiNotification::TurnStarted(
        octos_core::ui_protocol::TurnStartedEvent {
            session_id: session_id.clone(),
            turn_id: real_turn_id.clone(),
            timestamp: Utc::now(),
            topic: None,
        },
    ));
    let real_terminal = ledger.append_notification(UiNotification::TurnError(TurnErrorEvent {
        session_id: session_id.clone(),
        topic: None,
        turn_id: real_turn_id.clone(),
        code: "provider_failed".into(),
        message: "provider failed".into(),
        token_usage: None,
        partial_result: None,
    }));
    handle_turn_state_get(
        &ws,
        &state,
        &ledger,
        &active_turns,
        None,
        None,
        ConnectionUiFeatures::stdio_defaults(),
        "real-error".into(),
        TurnStateGetParams {
            session_id: session_id.clone(),
            turn_id: real_turn_id.clone(),
        },
    )
    .await;
    let real_frame = recv_rpc_json(&mut rx).await;
    assert_eq!(real_frame["result"]["state"], "errored");
    assert_eq!(real_frame["result"]["completed_at_durable"], true);

    let (events, _) = ledger.snapshot_with_cursor(&session_id, None).unwrap();
    let turns = collect_session_turns(&session_id, &active_turns, &events, &[]).await;
    assert_eq!(turns.len(), 2);
    let recovered = turns.iter().find(|turn| turn.turn_id == turn_id).unwrap();
    assert_eq!(recovered.state, TurnLifecycleState::Errored);
    assert_eq!(recovered.completed_at, terminal.recorded_at);
    assert_eq!(recovered.completed_at_durable, None);
    let real = turns
        .iter()
        .find(|turn| turn.turn_id == real_turn_id)
        .unwrap();
    assert_eq!(real.state, TurnLifecycleState::Errored);
    assert_eq!(real.completed_at, real_terminal.recorded_at);
    assert_eq!(real.completed_at_durable, Some(true));
}
