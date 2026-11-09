// Included by ui_protocol_tests.rs; uses the existing transport fixtures.


#[tokio::test]
async fn session_hydrate_file_mutations_honor_include_after_and_profile_scope() {
    use octos_core::ui_protocol::{UiFileMutationNotice, UiProgressMetadata};
    let temp = tempfile::tempdir().unwrap();
    let state = state_with_sessions(temp.path());
    let ledger = event_ledger(&state).await;
    let session = SessionKey("local:hydrate-file-changes".into());
    state
        .sessions
        .as_ref()
        .unwrap()
        .lock()
        .await
        .get_or_create(&session)
        .await;
    let mut notice = UiFileMutationNotice::new("sum.js", "modify");
    notice.preview_id = Some(PreviewId::new());
    let entry = ledger.append_progress(UiProgressEvent::new(
        session.clone(),
        None,
        UiProgressMetadata::file_mutation(notice.clone()),
    ));
    for (include, expected) in [
        (vec![], true),
        (vec!["messages".into()], true),
        (vec!["turns".into()], false),
    ] {
        let (ws, mut rx) = ws_connection_for_test(8);
        handle_session_hydrate(
            &ws,
            &state,
            &ledger,
            &PendingApprovalStore::default(),
            &PendingQuestionStore::default(),
            &active_turns_registry(),
            None,
            None,
            ConnectionUiFeatures::default(),
            "hydrate".into(),
            SessionHydrateParams {
                session_id: session.clone(),
                after: Some(entry.cursor.clone()),
                include,
            },
        )
        .await;
        let frame = recv_rpc_json(&mut rx).await;
        assert!(frame.get("error").is_none(), "{frame}");
        if expected {
            assert_eq!(frame["result"]["file_mutations"], json!([notice]));
        } else {
            assert!(frame["result"].get("file_mutations").is_none());
        }
    }
    let (ws, mut rx) = ws_connection_for_test(8);
    handle_session_hydrate(
        &ws,
        &state,
        &ledger,
        &PendingApprovalStore::default(),
        &PendingQuestionStore::default(),
        &active_turns_registry(),
        Some("other-profile"),
        None,
        ConnectionUiFeatures::default(),
        "forbidden".into(),
        SessionHydrateParams {
            session_id: SessionKey::with_profile("coding", "local", "private"),
            after: None,
            include: vec![],
        },
    )
    .await;
    // A profile mismatch follows the transport's existing fail-closed close path.
    assert!(matches!(rx.recv().await, Some(WsMessage::Close(Some(frame))) if frame.code == 1008));
}
