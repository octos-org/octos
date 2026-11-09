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
