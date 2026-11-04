fn blocked_ledger_for_test() -> (tempfile::TempDir, UiProtocolLedger) {
    let temp = tempfile::tempdir().expect("tempdir");
    let blocked = temp.path().join("blocked-ledger-data-dir");
    std::fs::write(&blocked, b"not a directory").expect("block ledger data dir");
    (
        temp,
        UiProtocolLedger::with_config(LedgerConfig::durable(blocked)),
    )
}
#[tokio::test]
async fn should_fail_terminal_send_when_ledger_persistence_fails() {
    let (ws, mut rx) = ws_connection_for_test(8);
    let (_temp, ledger) = blocked_ledger_for_test();
    let session_id = SessionKey("local:d01-terminal".into());
    let result = send_notification_lifecycle(
        &ws,
        &ledger,
        UiNotification::TurnCompleted(TurnCompletedEvent {
            session_id,
            topic: None,
            turn_id: TurnId::new(),
            cursor: None,
            tokens_in: None,
            tokens_out: None,
            session_result: None,
            token_usage: None,
        }),
    );
    assert!(matches!(result, Err(SendError::LifecycleFailure(_))));
    assert!(
        rx.try_recv().is_err(),
        "a terminal whose durable append failed must not reach the writer"
    );
}
#[tokio::test]
async fn should_fail_approval_send_when_ledger_persistence_fails() {
    let (ws, mut rx) = ws_connection_for_test(8);
    let (_temp, ledger) = blocked_ledger_for_test();
    let session_id = SessionKey("local:d01-approval".into());
    let result = send_notification_durable(
        &ws,
        &ledger,
        UiNotification::ApprovalCancelled(ApprovalCancelledEvent {
            session_id: session_id.clone(),
            topic: None,
            approval_id: ApprovalId::new(),
            turn_id: TurnId::new(),
            reason: "test_persistence_failure".into(),
        }),
    );
    assert!(matches!(result, Err(SendError::LifecycleFailure(_))));
    assert!(
        rx.try_recv().is_err(),
        "an approval event whose durable append failed must not reach the writer"
    );
}
#[tokio::test]
async fn should_keep_approval_pending_and_unwoken_when_decided_append_fails() {
    let (ws, mut rx) = ws_connection_for_test(8);
    let (_temp, ledger) = blocked_ledger_for_test();
    let ledger = Arc::new(ledger);
    let state = state_with_sessions(_temp.path());
    let contracts = Arc::new(UiProtocolContractStores::default());
    let session_id = SessionKey("local:d01-approval-rpc".into());
    let approval_id = ApprovalId::new();
    let turn_id = TurnId::new();
    let mut runtime_rx = contracts
        .approvals
        .request_runtime(ApprovalRequestedEvent::generic(
            session_id.clone(),
            approval_id.clone(),
            turn_id,
            "shell",
            "Run",
            "body",
        ));
    let params =
        ApprovalRespondParams::new(session_id.clone(), approval_id, ApprovalDecision::Approve);
    handle_approval_respond(
        &ws,
        &state,
        &ledger,
        &contracts,
        None,
        None,
        "d01-approval-rpc".into(),
        params,
    )
    .await;
    let frame = rx.try_recv().expect("approval RPC error frame");
    let axum::extract::ws::Message::Text(text) = frame else {
        panic!("expected JSON-RPC text frame");
    };
    let value: Value = serde_json::from_str(&text).expect("parse RPC error");
    assert_eq!(
        value["error"]["code"],
        json!(rpc_error_codes::INTERNAL_ERROR)
    );
    assert!(value.get("result").is_none());
    assert!(
        runtime_rx.try_recv().is_err(),
        "unlogged approval must not wake execution"
    );
    assert_eq!(
        contracts.approvals.pending_for_session(&session_id).len(),
        1
    );
}

#[tokio::test]
async fn should_not_ack_interrupt_when_terminal_durable_append_fails() {
    let (ws, mut rx) = ws_connection_for_test(8);
    let (_temp, ledger) = blocked_ledger_for_test();
    let session_id = SessionKey("local:d01-interrupt-ack".into());
    let (ack, ack_rx) = tokio::sync::oneshot::channel();
    let turn_state = TokioMutex::new(TurnState::Interrupting {
        ack,
        origin: InterruptOrigin::Client,
    });
    let result = try_emit_terminal(
        &turn_state,
        TerminalReason::Completed,
        &ws,
        &ledger,
        &session_id,
        &TurnId::new(),
        None,
        None,
        None,
        None,
    )
    .await;
    assert!(matches!(result, Err(SendError::LifecycleFailure(_))));
    assert!(ack_rx.await.is_err(), "a rejected terminal must not ACK");
    assert!(rx.try_recv().is_err(), "no terminal frame may be sent");
}

#[tokio::test]
async fn should_not_ack_connection_abort_when_terminal_append_fails() {
    let (_temp, ledger) = blocked_ledger_for_test();
    let session_id = SessionKey("local:d01-abort-ack".into());
    let turn_id = TurnId::new();
    let (ack, ack_rx) = tokio::sync::oneshot::channel();
    let state = Arc::new(TokioMutex::new(TurnState::Interrupting {
        ack,
        origin: InterruptOrigin::Client,
    }));
    let active_turns: SharedActiveTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let connection_turns: SharedConnectionTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let handle = tokio::spawn(std::future::pending::<()>());
    let mut active = test_active_turn(turn_id.clone(), handle.abort_handle());
    active.state = state;
    active_turns.lock().await.insert(session_id.clone(), active);
    connection_turns.lock().await.insert(
        session_id.clone(),
        test_connection_turn(&active_turns, &session_id, &turn_id).await,
    );
    abort_connection_turns(
        &active_turns,
        &connection_turns,
        &ScopePolicy::default(),
        &ledger,
        &PendingApprovalStore::default(),
        &PendingQuestionStore::default(),
    )
    .await;
    assert!(
        ack_rx.await.is_err(),
        "connection abort must not ACK a terminal that was not logged"
    );
}

