


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
