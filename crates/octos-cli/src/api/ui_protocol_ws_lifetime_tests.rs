//! Ordinary WS transport loss is not cancellation of admitted server work.
use super::*;

// The real tool fixture exercises Unix session-workspace path confinement.
#[cfg(unix)]
#[path = "ui_protocol_ingress_lifetime_tests.rs"]
mod ingress_lifetime_tests;

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
