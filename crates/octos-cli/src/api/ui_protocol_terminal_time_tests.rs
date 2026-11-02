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
